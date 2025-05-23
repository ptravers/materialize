# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ set-arg-default single-replica-cluster=quickstart

# Tests that hit the Confluent Schema Registry.

> CREATE CONNECTION IF NOT EXISTS csr_conn TO CONFLUENT SCHEMA REGISTRY (
    URL '${testdrive.schema-registry-url}'
  );

# Verify the error message is useful when a schema is not present in the
# registry.

> CREATE CONNECTION kafka_conn
  TO KAFKA (BROKER '${testdrive.kafka-addr}', SECURITY PROTOCOL PLAINTEXT);

$ kafka-create-topic topic=noexist partitions=1

> CREATE SOURCE noexist
  IN CLUSTER ${arg.single-replica-cluster}
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-noexist-${testdrive.seed}')
! CREATE TABLE noexist_tbl FROM SOURCE noexist (REFERENCE "testdrive-noexist-${testdrive.seed}")
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM
contains:No value schema found

$ set schema-v1={
    "type": "record",
    "name": "envelope",
    "fields": [
      {
        "name": "before",
        "type": [
          {
            "name": "row",
            "type": "record",
            "fields": [
              {"name": "a", "type": "long"}
            ]
          },
          "null"
        ]
      },
      { "name": "op", "type": "string" },
      { "name": "after", "type": ["row", "null"] },
      {
        "name": "source",
        "type": {
          "type": "record",
          "name": "Source",
          "namespace": "io.debezium.connector.mysql",
          "fields": [
            {
              "name": "file",
              "type": "string"
            },
            {
              "name": "pos",
              "type": "long"
            },
            {
              "name": "row",
              "type": "int"
            },
            {
              "name": "snapshot",
              "type": [
                {
                  "type": "boolean",
                  "connect.default": false
                },
                "null"
              ],
              "default": false
            }
          ],
          "connect.name": "io.debezium.connector.mysql.Source"
        }
      }
    ]
  }

$ set schema-v2={
    "type": "record",
    "name": "envelope",
    "fields": [
      {
        "name": "before",
        "type": [
          {
            "name": "row",
            "type": "record",
            "fields": [
              {"name": "a", "type": "long"},
              {"name": "b", "type": "long", "default": 42}
            ]
          },
          "null"
        ]
      },
      { "name": "op", "type": "string" },
      { "name": "after", "type": ["row", "null"] },
      {
        "name": "source",
        "type": {
          "type": "record",
          "name": "Source",
          "namespace": "io.debezium.connector.mysql",
          "fields": [
            {
              "name": "file",
              "type": "string"
            },
            {
              "name": "pos",
              "type": "long"
            },
            {
              "name": "row",
              "type": "int"
            },
            {
              "name": "snapshot",
              "type": [
                {
                  "type": "boolean",
                  "connect.default": false
                },
                "null"
              ],
              "default": false
            }
          ],
          "connect.name": "io.debezium.connector.mysql.Source"
        }
      }
    ]
  }

$ kafka-create-topic topic=data

$ set valid-key-schema={
    "type": "record",
    "name": "Key",
    "fields": [
        {"name": "a", "type": "long"}
    ]
  }

$ kafka-ingest format=avro topic=data key-format=avro key-schema=${valid-key-schema} schema=${schema-v1} timestamp=1
{"a": 1} {"before": null, "after": {"row": {"a": 1}}, "source": {"file": "binlog", "pos": 1, "row": 1, "snapshot": {"boolean": false}}, "op": "c"}

> CREATE SOURCE data_v1
  IN CLUSTER ${arg.single-replica-cluster}
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-data-${testdrive.seed}')

> CREATE TABLE data_v1_tbl FROM SOURCE data_v1 (REFERENCE "testdrive-data-${testdrive.seed}")
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

> SELECT * FROM data_v1_tbl
a
---
1

$ kafka-ingest format=avro topic=data key-format=avro key-schema=${valid-key-schema} schema=${schema-v2} timestamp=3
{"a": 2} {"before": null, "after": {"row": {"a": 2, "b": -1}}, "source": {"file": "binlog2", "pos": 1, "row": 1, "snapshot": {"boolean": false}}, "op": "c"}

> CREATE SOURCE data_v2
  IN CLUSTER ${arg.single-replica-cluster}
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-data-${testdrive.seed}')

> CREATE TABLE data_v2_tbl FROM SOURCE data_v2 (REFERENCE "testdrive-data-${testdrive.seed}")
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

> SELECT * FROM data_v1_tbl
a
---
1
2

> SELECT * FROM data_v2_tbl
a b
----
1 42
2 -1

# [btv] uncomment if we bring back classic debezium mode
# $ kafka-ingest topic=data timestamp=5
#   format=avro schema=${schema-v1} key-format=avro key-schema=${valid-key-schema}
# {"a": 1} {"before": null, "after": {"row": {"a": 1}}, "source": {"file": "binlog3", "pos": 1, "row": 1, "snapshot": {"boolean": false}}, "op": "c"}

# > CREATE SOURCE data_v3
#   IN CLUSTER ${arg.single-replica-cluster}
#   FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-data-${testdrive.seed}')

# > CREATE TABLE data_v3_tbl FROM SOURCE data_v3 (REFERENCE "testdrive-data-${testdrive.seed}")
#   FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
#   ENVELOPE DEBEZIUM

# > SELECT * FROM data_v3_tbl
# a b
# ----
# 1 42
# 1 42
# 2 -1

# # Make sure this query gives WRONG results,
# # which should prove that we are respecting primary
# # key information (and optimizing by transforming
# # a reduce on a primary key to a map)

# > SELECT a, count(*) FROM data_v3_tbl
#   GROUP BY a
# a count
# -------
# 1 1
# 1 1
# 2 1
