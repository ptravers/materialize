# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

# Test reporting of dataflow hydration status through
# `mz_internal.mz_compute_hydration_statuses`,
# `mz_internal.mz_hydration_statuses`, and
# `mz_internal.mz_compute_operator_hydration_statuses`.
#
# Note that all of the below tests only assert that the `hydrated` flag
# eventually becomes `true`, not that it starts off as `false`. That's because
# we have no control about the hydration timing of dataflows or the update
# cadence of the hydration introspection relations, so we have no reliable way
# of ensuring that a query arrives before a dataflow has hydrated.
#
# These tests rely on testdrive's retry feature, as dataflows can take an
# unknown (but hopefully small) time to hydrate. Furthermore, the queried
# introspection relations are asynchronously updated, so DDL commands are not
# immediately reflected there.

> CREATE CLUSTER test REPLICAS (hydrated_test_1 (SIZE '1'))
> SET cluster = test

# Test that on an empty cluster only the introspection indexes show up.
# Introspection subscribes do not show up because the hydration introspection
# relations exclude transient dataflow.

> SELECT DISTINCT left(h.object_id, 2), h.hydrated
  FROM mz_internal.mz_compute_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  WHERE r.name LIKE 'hydrated_test%';
si true

> SELECT DISTINCT left(h.object_id, 2), h.hydrated
  FROM mz_internal.mz_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  WHERE r.name LIKE 'hydrated_test%';
si true

# No operator-level hydration status logging for introspection dataflows.
> SELECT DISTINCT left(h.object_id, 1), h.hydrated
  FROM mz_internal.mz_compute_operator_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  WHERE r.name LIKE 'hydrated_test%';

# Test adding new compute dataflows.

> CREATE TABLE t (a int)
> CREATE INDEX idx ON t (a)
> CREATE MATERIALIZED VIEW mv AS SELECT * FROM t
> CREATE MATERIALIZED VIEW mv_const AS SELECT 1

> SELECT o.name, r.name, h.hydrated, h.hydration_time IS NOT NULL
  FROM mz_internal.mz_compute_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%';
idx      hydrated_test_1 true true
mv       hydrated_test_1 true true
mv_const hydrated_test_1 true false

> SELECT o.name, r.name, h.hydrated
  FROM mz_internal.mz_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%';
idx      hydrated_test_1 true
mv       hydrated_test_1 true
mv_const hydrated_test_1 true

> SELECT o.name, r.name, bool_and(h.hydrated)
  FROM mz_internal.mz_compute_operator_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%'
  GROUP BY o.name, r.name;
idx      hydrated_test_1 true
mv       hydrated_test_1 true
mv_const hydrated_test_1 true

# Test adding new replicas.

> CREATE CLUSTER REPLICA test.hydrated_test_2 SIZE '1'

> SELECT o.name, r.name, h.hydrated, h.hydration_time IS NOT NULL
  FROM mz_internal.mz_compute_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%';
idx      hydrated_test_1 true true
idx      hydrated_test_2 true true
mv       hydrated_test_1 true true
mv       hydrated_test_2 true true
mv_const hydrated_test_1 true false
mv_const hydrated_test_2 true false

> SELECT o.name, r.name, h.hydrated
  FROM mz_internal.mz_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%';
idx      hydrated_test_1 true
idx      hydrated_test_2 true
mv       hydrated_test_1 true
mv       hydrated_test_2 true
mv_const hydrated_test_1 true
mv_const hydrated_test_2 true

# `mv_const` doesn't show up for the second replica because its output was
# already fully computed so it wasn't installed on the replica and no operators
# exist.
> SELECT o.name, r.name, bool_and(h.hydrated)
  FROM mz_internal.mz_compute_operator_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%'
  GROUP BY o.name, r.name;
idx      hydrated_test_1 true
idx      hydrated_test_2 true
mv       hydrated_test_1 true
mv       hydrated_test_2 true
mv_const hydrated_test_1 true

# Test dropping replicas.

> DROP CLUSTER REPLICA test.hydrated_test_1

> SELECT o.name, r.name, h.hydrated, h.hydration_time IS NOT NULL
  FROM mz_internal.mz_compute_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%';
idx      hydrated_test_2 true true
mv       hydrated_test_2 true true
mv_const hydrated_test_2 true false

> SELECT o.name, r.name, h.hydrated
  FROM mz_internal.mz_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%';
idx      hydrated_test_2 true
mv       hydrated_test_2 true
mv_const hydrated_test_2 true

> SELECT o.name, r.name, bool_and(h.hydrated)
  FROM mz_internal.mz_compute_operator_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%'
  GROUP BY o.name, r.name;
idx      hydrated_test_2 true
mv       hydrated_test_2 true

> DROP CLUSTER REPLICA test.hydrated_test_2

> SELECT o.name, r.name, h.hydrated, h.hydration_time IS NOT NULL
  FROM mz_internal.mz_compute_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%';

> SELECT o.name, r.name, h.hydrated
  FROM mz_internal.mz_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%';

> SELECT o.name, r.name, bool_and(h.hydrated)
  FROM mz_internal.mz_compute_operator_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%'
  GROUP BY o.name, r.name;

# Test dropping dataflows.

> CREATE CLUSTER REPLICA test.hydrated_test_3 SIZE '1'

> SELECT o.name, r.name, h.hydrated, h.hydration_time IS NOT NULL
  FROM mz_internal.mz_compute_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%';
idx      hydrated_test_3 true true
mv       hydrated_test_3 true true
mv_const hydrated_test_3 true false

> SELECT o.name, r.name, h.hydrated
  FROM mz_internal.mz_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%';
idx      hydrated_test_3 true
mv       hydrated_test_3 true
mv_const hydrated_test_3 true

> SELECT o.name, r.name, bool_and(h.hydrated)
  FROM mz_internal.mz_compute_operator_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%'
  GROUP BY o.name, r.name;
idx      hydrated_test_3 true
mv       hydrated_test_3 true

> DROP INDEX idx;
> DROP MATERIALIZED VIEW mv_const;

> SELECT o.name, r.name, h.hydrated, h.hydration_time IS NOT NULL
  FROM mz_internal.mz_compute_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%';
mv  hydrated_test_3 true true

> SELECT o.name, r.name, h.hydrated
  FROM mz_internal.mz_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%';
mv  hydrated_test_3 true

> SELECT o.name, r.name, bool_and(h.hydrated)
  FROM mz_internal.mz_compute_operator_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%'
  GROUP BY o.name, r.name;
mv  hydrated_test_3 true

# Test adding new storage dataflows.

> CREATE SOURCE src
  IN CLUSTER test
  FROM LOAD GENERATOR auction (UP TO 100);

> CREATE TABLE accounts FROM SOURCE src (REFERENCE accounts);
> CREATE TABLE auctions FROM SOURCE src (REFERENCE auctions);
> CREATE TABLE bids FROM SOURCE src (REFERENCE bids);
> CREATE TABLE organizations FROM SOURCE src (REFERENCE organizations);
> CREATE TABLE users FROM SOURCE src (REFERENCE users);

> CREATE CONNECTION kafka_conn
  TO KAFKA (BROKER '${testdrive.kafka-addr}', SECURITY PROTOCOL PLAINTEXT)
> CREATE CONNECTION csr_conn
  TO CONFLUENT SCHEMA REGISTRY (URL '${testdrive.schema-registry-url}')
> CREATE SINK snk
  IN CLUSTER test
  FROM mv
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-sink1-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

# Webhook sources are not backed by dataflows, so they have no concept of
# hydration and shouldn't show up in mz_hydration_statuses.
> CREATE SOURCE web IN CLUSTER test FROM WEBHOOK BODY FORMAT JSON

> SELECT o.name, r.name, h.hydrated
  FROM mz_internal.mz_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%';
mv            hydrated_test_3 true
src           hydrated_test_3 true
snk           hydrated_test_3 true

# Test dropping replicas.

> DROP CLUSTER REPLICA test.hydrated_test_3

> SELECT o.name, r.name, h.hydrated
  FROM mz_internal.mz_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%';

# Test adding new replicas.

> CREATE CLUSTER REPLICA test.hydrated_test_4 SIZE '1'

> SELECT o.name, r.name, h.hydrated
  FROM mz_internal.mz_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%';
mv            hydrated_test_4 true
src           hydrated_test_4 true
snk           hydrated_test_4 true

# Test dropping dataflows.

> DROP SINK snk
> DROP SOURCE src CASCADE
> DROP MATERIALIZED VIEW mv

> SELECT o.name, r.name, h.hydrated
  FROM mz_internal.mz_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%';

# Test hydration status reporting with WMR dataflows.

> CREATE MATERIALIZED VIEW mv_wmr AS
  WITH MUTUALLY RECURSIVE
      x (a int) AS (
          SELECT * FROM t
          UNION ALL
          SELECT a + 1 FROM x WHERE a < 10
      )
  SELECT * FROM x;
> CREATE MATERIALIZED VIEW mv_wmr_const AS
  WITH MUTUALLY RECURSIVE
      x (a int) AS (
          VALUES (1)
          UNION ALL
          SELECT a + 1 FROM x WHERE a < 10
      )
  SELECT * FROM x;
> CREATE MATERIALIZED VIEW mv_wmr_stuck AS
  WITH MUTUALLY RECURSIVE
      x (a int) AS (
          VALUES (1)
          UNION ALL
          SELECT a + 1 FROM x
      )
  SELECT * FROM x;

> SELECT o.name, r.name, h.hydrated, h.hydration_time IS NOT NULL
  FROM mz_internal.mz_compute_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%';
mv_wmr       hydrated_test_4 true  true
mv_wmr_const hydrated_test_4 true  false
mv_wmr_stuck hydrated_test_4 false false

> SELECT o.name, r.name, h.hydrated
  FROM mz_internal.mz_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%';
mv_wmr       hydrated_test_4 true
mv_wmr_const hydrated_test_4 true
mv_wmr_stuck hydrated_test_4 false

> SELECT o.name, r.name, bool_and(h.hydrated)
  FROM mz_internal.mz_compute_operator_hydration_statuses h
  JOIN mz_cluster_replicas r ON (r.id = h.replica_id)
  JOIN mz_objects o ON (o.id = h.object_id)
  WHERE
      r.name LIKE 'hydrated_test%' AND
      o.id NOT LIKE 's%'
  GROUP BY o.name, r.name;
mv_wmr       hydrated_test_4 true
mv_wmr_const hydrated_test_4 true
mv_wmr_stuck hydrated_test_4 false

> DROP MATERIALIZED VIEW mv_wmr
> DROP MATERIALIZED VIEW mv_wmr_const
> DROP MATERIALIZED VIEW mv_wmr_stuck

# Test that incorrectly configured sinks do _not_ show as hydrated.

> CREATE TABLE schema1 (a int)
> CREATE TABLE schema2 (a text)

> CREATE SINK snk_schema1
  IN CLUSTER test
  FROM schema1
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk_schema1-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

> SELECT s.name, h.hydrated
  FROM mz_sinks s
  JOIN mz_internal.mz_hydration_statuses h ON (h.object_id = s.id)
snk_schema1 true

> CREATE SINK snk_schema2
  IN CLUSTER test
  FROM schema2
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk_schema1-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

# If we have a bug, the sink's hydration status might toggle between `false`
# and `true`, rather than always being `true`. Using retries might therefore
# cause this test to pass even if it shouldn't. We instead disable retries and
# manually check the hydration status a couple times.
$ set-max-tries max-tries=1

> SELECT s.name, h.hydrated
  FROM mz_sinks s
  JOIN mz_internal.mz_hydration_statuses h ON (h.object_id = s.id)
snk_schema1 true
snk_schema2 false

> SELECT s.name, h.hydrated
  FROM mz_sinks s
  JOIN mz_internal.mz_hydration_statuses h ON (h.object_id = s.id)
snk_schema1 true
snk_schema2 false

> SELECT s.name, h.hydrated
  FROM mz_sinks s
  JOIN mz_internal.mz_hydration_statuses h ON (h.object_id = s.id)
snk_schema1 true
snk_schema2 false

> SELECT s.name, h.hydrated
  FROM mz_sinks s
  JOIN mz_internal.mz_hydration_statuses h ON (h.object_id = s.id)
snk_schema1 true
snk_schema2 false

> SELECT s.name, h.hydrated
  FROM mz_sinks s
  JOIN mz_internal.mz_hydration_statuses h ON (h.object_id = s.id)
snk_schema1 true
snk_schema2 false
