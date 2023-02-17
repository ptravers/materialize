// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

#![allow(missing_docs)]

//! Types for commands to clusters.

use std::num::NonZeroI64;

use proptest::prelude::{any, Arbitrary};
use proptest::strategy::{BoxedStrategy, Strategy};
use proptest_derive::Arbitrary;
use serde::{Deserialize, Serialize};

use mz_proto::{ProtoType, RustType, TryFromProtoError};

include!(concat!(env!("OUT_DIR"), "/mz_cluster_client.client.rs"));

/// A value generated by environmentd and passed to the clusterd processes
/// to help them disambiguate different `CreateTimely` commands.
///
/// The semantics of this value are not important, except that they
/// must be totally ordered, and any value (for a given replica) must
/// be greater than any that were generated before (for that replica).
/// This is the reason for having two
/// components (one from the stash that increases on every environmentd restart,
/// another in-memory and local to the current incarnation of environmentd)
#[derive(PartialEq, Eq, Debug, Copy, Clone, Serialize, Deserialize)]
pub struct ClusterStartupEpoch {
    envd: NonZeroI64,
    replica: u64,
}

impl RustType<ProtoClusterStartupEpoch> for ClusterStartupEpoch {
    fn into_proto(&self) -> ProtoClusterStartupEpoch {
        let Self { envd, replica } = self;
        ProtoClusterStartupEpoch {
            envd: envd.get(),
            replica: *replica,
        }
    }

    fn from_proto(proto: ProtoClusterStartupEpoch) -> Result<Self, TryFromProtoError> {
        let ProtoClusterStartupEpoch { envd, replica } = proto;
        Ok(Self {
            envd: envd.try_into().unwrap(),
            replica,
        })
    }
}

impl Arbitrary for ClusterStartupEpoch {
    type Strategy = BoxedStrategy<Self>;
    type Parameters = ();

    fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
        (any::<i64>(), any::<u64>())
            .prop_map(|(envd, replica)| ClusterStartupEpoch {
                envd: NonZeroI64::new(if envd == 0 { envd + 1 } else { envd }).unwrap(),
                replica,
            })
            .boxed()
    }
}

impl ClusterStartupEpoch {
    pub fn new(envd: NonZeroI64, replica: u64) -> Self {
        Self { envd, replica }
    }

    /// Serialize for transfer over the network
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut ret = [0; 16];
        let mut p = &mut ret[..];
        use std::io::Write;
        p.write_all(&self.envd.get().to_be_bytes()[..]).unwrap();
        p.write_all(&self.replica.to_be_bytes()[..]).unwrap();
        ret
    }

    /// Inverse of `to_bytes`
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        let envd = i64::from_be_bytes((&bytes[0..8]).try_into().unwrap());
        let replica = u64::from_be_bytes((&bytes[8..16]).try_into().unwrap());
        Self {
            envd: envd.try_into().unwrap(),
            replica,
        }
    }

    pub fn envd(&self) -> NonZeroI64 {
        self.envd
    }

    pub fn replica(&self) -> u64 {
        self.replica
    }
}

impl std::fmt::Display for ClusterStartupEpoch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { envd, replica } = self;
        write!(f, "({envd}, {replica})")
    }
}

impl PartialOrd for ClusterStartupEpoch {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ClusterStartupEpoch {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let Self { envd, replica } = self;
        let Self {
            envd: other_envd,
            replica: other_replica,
        } = other;
        (envd, replica).cmp(&(other_envd, other_replica))
    }
}

/// Configuration of the cluster we will spin up
#[derive(Arbitrary, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TimelyConfig {
    /// Number of per-process worker threads
    pub workers: usize,
    /// Identity of this process
    pub process: usize,
    /// Addresses of all processes
    pub addresses: Vec<String>,
    /// The amount of effort to be spent on arrangement compaction during idle times.
    ///
    /// See `differential_dataflow::Config::idle_merge_effort`.
    pub idle_arrangement_merge_effort: u32,
}

impl RustType<ProtoTimelyConfig> for TimelyConfig {
    fn into_proto(&self) -> ProtoTimelyConfig {
        ProtoTimelyConfig {
            workers: self.workers.into_proto(),
            addresses: self.addresses.into_proto(),
            process: self.process.into_proto(),
            idle_arrangement_merge_effort: self.idle_arrangement_merge_effort,
        }
    }

    fn from_proto(proto: ProtoTimelyConfig) -> Result<Self, TryFromProtoError> {
        Ok(Self {
            process: proto.process.into_rust()?,
            workers: proto.workers.into_rust()?,
            addresses: proto.addresses.into_rust()?,
            idle_arrangement_merge_effort: proto.idle_arrangement_merge_effort,
        })
    }
}

impl TimelyConfig {
    pub fn split_command(&self, parts: usize) -> Vec<Self> {
        (0..parts)
            .into_iter()
            .map(|part| TimelyConfig {
                process: part,
                ..self.clone()
            })
            .collect()
    }
}

/// Specifies the location of a cluster replica.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterReplicaLocation {
    /// The network addresses of the cluster control endpoints for each process in
    /// the replica. Connections from the controller to these addresses
    /// are sent commands, and send responses back.
    pub ctl_addrs: Vec<String>,
    /// The network addresses of the dataflow (Timely) endpoints for
    /// each process in the replica. These are used for _internal_
    /// networking, that is, timely worker communicating messages
    /// between themselves.
    pub dataflow_addrs: Vec<String>,
    /// The workers per process in the replica.
    pub workers: usize,
}

#[cfg(test)]
mod tests {
    use proptest::prelude::ProptestConfig;
    use proptest::proptest;

    use mz_proto::protobuf_roundtrip;

    use super::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn timely_config_protobuf_roundtrip(expect in any::<TimelyConfig>() ) {
            let actual = protobuf_roundtrip::<_, ProtoTimelyConfig>(&expect);
            assert!(actual.is_ok());
            assert_eq!(actual.unwrap(), expect);
        }

        #[test]
        fn cluster_startup_epoch_protobuf_roundtrip(expect in any::<ClusterStartupEpoch>() ) {
            let actual = protobuf_roundtrip::<_, ProtoClusterStartupEpoch>(&expect);
            assert!(actual.is_ok());
            assert_eq!(actual.unwrap(), expect);
        }
    }
}