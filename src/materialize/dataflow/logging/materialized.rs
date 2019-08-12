// Copyright 2019 Materialize, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Materialize, Inc.

use super::{BatchLogger, LogVariant, MaterializedLog};
use crate::dataflow::arrangement::KeysOnlyHandle;
use crate::dataflow::types::Timestamp;
use crate::repr::Datum;
use std::time::Duration;
use timely::communication::Allocate;
use timely::dataflow::operators::capture::EventLink;
use timely::dataflow::operators::generic::operator::Operator;
use timely::dataflow::operators::probe::Probe;
use timely::dataflow::ProbeHandle;
use timely::logging::WorkerIdentifier;

/// Type alias for logging of materialized events.
pub type Logger = timely::logging_core::Logger<MaterializedEvent, WorkerIdentifier>;

/// A logged materialized event.
#[derive(Clone, Ord, PartialOrd, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub enum MaterializedEvent {
    /// Dataflow command, true for create and false for drop.
    Dataflow(String, bool),
    /// Peek command, true for install and false for retire.
    Peek(Peek, bool),
    /// Available frontier information for views.
    Frontier(String, Timestamp, i64),
}

/// A logged peek event.
#[derive(Clone, Ord, PartialOrd, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Peek {
    /// The name of the view the peek targets.
    name: String,
    /// The logical timestamp requested.
    time: Timestamp,
    /// The UUID of the peek.
    uuid: uuid::Uuid,
}

impl Peek {
    pub fn new(name: &str, time: Timestamp, uuid: &uuid::Uuid) -> Self {
        Self {
            name: name.to_string(),
            time,
            uuid: *uuid,
        }
    }
}

pub fn construct<A: Allocate>(
    worker: &mut timely::worker::Worker<A>,
    probe: &mut ProbeHandle<Timestamp>,
    config: &super::LoggingConfiguration,
) -> (
    BatchLogger<
        MaterializedEvent,
        WorkerIdentifier,
        std::rc::Rc<EventLink<Timestamp, (Duration, WorkerIdentifier, MaterializedEvent)>>,
    >,
    std::collections::HashMap<LogVariant, KeysOnlyHandle>,
) {
    // Create timely dataflow logger based on shared linked lists.
    let writer = EventLink::<Timestamp, (Duration, WorkerIdentifier, MaterializedEvent)>::new();
    let writer = std::rc::Rc::new(writer);
    let reader = writer.clone();

    let granularity_ns = config.granularity_ns as u64;

    // The two return values.
    let logger = BatchLogger::new(writer);

    let traces = worker.dataflow(move |scope| {
        use differential_dataflow::collection::AsCollection;
        use differential_dataflow::operators::arrange::arrangement::ArrangeBySelf;
        use timely::dataflow::operators::capture::Replay;
        use timely::dataflow::operators::Map;

        // TODO: Rewrite as one operator with multiple outputs.
        let logs = Some(reader).replay_into(scope);

        use timely::dataflow::operators::generic::builder_rc::OperatorBuilder;

        let mut demux =
            OperatorBuilder::new("Materialize Logging Demux".to_string(), scope.clone());
        use timely::dataflow::channels::pact::Pipeline;
        let mut input = demux.new_input(&logs, Pipeline);
        let (mut dataflow_out, dataflow) = demux.new_output();
        let (mut peek_out, peek) = demux.new_output();
        let (mut frontier_out, frontier) = demux.new_output();
        let mut demux_buffer = Vec::new();
        demux.build(move |_capability| {
            move |_frontiers| {
                let mut dataflow = dataflow_out.activate();
                let mut peek = peek_out.activate();
                let mut frontier = frontier_out.activate();

                input.for_each(|time, data| {
                    data.swap(&mut demux_buffer);

                    let mut dataflow_session = dataflow.session(&time);
                    let mut peek_session = peek.session(&time);
                    let mut frontier_session = frontier.session(&time);

                    for (time, worker, datum) in demux_buffer.drain(..) {
                        let time = time.as_nanos() as Timestamp;

                        match datum {
                            MaterializedEvent::Dataflow(name, is_create) => {
                                dataflow_session.give((name, worker, is_create, time))
                            }
                            MaterializedEvent::Peek(peek, is_install) => {
                                peek_session.give((peek, worker, is_install, time))
                            }
                            MaterializedEvent::Frontier(name, logical, delta) => {
                                frontier_session.give((name, logical, delta as isize, time))
                            }
                        }
                    }
                });
            }
        });

        let dataflow_current = dataflow
            .map(move |(name, worker, is_create, time)| {
                let time = ((time / granularity_ns) + 1) * granularity_ns;
                ((name, worker), time, if is_create { 1 } else { -1 })
            })
            .as_collection()
            .map(|(name, worker)| vec![Datum::String(name), Datum::Int64(worker as i64)])
            .arrange_by_self();
        dataflow_current.stream.probe_with(probe);

        let peek_current = peek
            .map(move |(name, worker, is_install, time)| {
                let time = ((time / granularity_ns) + 1) * granularity_ns;
                ((name, worker), time, if is_install { 1 } else { -1 })
            })
            .as_collection()
            .map(|(peek, worker)| {
                vec![
                    Datum::String(format!("{}", peek.uuid)),
                    Datum::Int64(worker as i64),
                    Datum::String(peek.name),
                    Datum::Int64(peek.time as i64),
                ]
            })
            .arrange_by_self();
        peek_current.stream.probe_with(probe);

        let frontier_current = frontier
            .map(move |(name, logical, delta, time)| {
                let time = ((time / granularity_ns) + 1) * granularity_ns;
                ((name, logical), time, delta)
            })
            .as_collection()
            .map(|(name, logical)| vec![Datum::String(name), Datum::Int64(logical as i64)])
            .arrange_by_self();
        frontier_current.stream.probe_with(probe);

        // Duration statistics derive from the non-rounded event times.
        use differential_dataflow::operators::reduce::Count;
        let peek_duration = peek
            .unary(
                timely::dataflow::channels::pact::Pipeline,
                "Peeks",
                |_, _| {
                    let mut map = std::collections::HashMap::new();
                    let mut vec = Vec::new();

                    move |input, output| {
                        input.for_each(|time, data| {
                            data.swap(&mut vec);
                            let mut session = output.session(&time);
                            for (peek, worker, is_install, time_ns) in vec.drain(..) {
                                let key = (worker, peek.uuid);
                                if is_install {
                                    assert!(!map.contains_key(&key));
                                    map.insert(key, time_ns);
                                } else {
                                    assert!(map.contains_key(&key));
                                    let start = map.remove(&key).expect("start event absent");
                                    let elapsed = time_ns - start;
                                    let time_ns = ((time_ns / granularity_ns) + 1) * granularity_ns;
                                    session.give((
                                        (key.0, elapsed.next_power_of_two()),
                                        time_ns,
                                        1isize,
                                    ));
                                }
                            }
                        });
                    }
                },
            )
            .as_collection()
            .count()
            .map(|((worker, pow), count)| {
                vec![
                    Datum::Int64(worker as i64),
                    Datum::Int64(pow as i64),
                    Datum::Int64(count as i64),
                ]
            })
            .arrange_by_self();

        peek_duration.stream.probe_with(probe);

        vec![
            (
                LogVariant::Materialized(MaterializedLog::DataflowCurrent),
                dataflow_current.trace,
            ),
            (
                LogVariant::Materialized(MaterializedLog::FrontierCurrent),
                frontier_current.trace,
            ),
            (
                LogVariant::Materialized(MaterializedLog::PeekCurrent),
                peek_current.trace,
            ),
            (
                LogVariant::Materialized(MaterializedLog::PeekDuration),
                peek_duration.trace,
            ),
        ]
        .into_iter()
        .filter(|(name, _trace)| config.active_logs.contains(name))
        .collect()
    });

    (logger, traces)
}
