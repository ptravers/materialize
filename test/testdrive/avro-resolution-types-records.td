# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ set-arg-default single-replica-cluster=quickstart

#
# Attempt to push a double value inside a record into a int source. According to the schema registry
# this is allowed, so it is up to Materialize to reject it.
#

$ set int-col={"type": "record", "name": "outer", "fields": [ {"name": "f1", "type": {"type": "record", "name": "inner", "fields": [ {"name": "f1", "type": "int" } ] } } ] }
$ set double-col={"type": "record", "name": "outer", "fields": [ {"name": "f1", "type": {"type": "record", "name": "inner", "fields": [ {"name": "f1", "type": "double" } ] } } ] }

$ kafka-create-topic topic=resolution-records-int2double

$ kafka-ingest format=avro topic=resolution-records-int2double schema=${int-col} timestamp=1
{"f1": { "f1": 123 }}

> CREATE CONNECTION IF NOT EXISTS csr_conn TO CONFLUENT SCHEMA REGISTRY (
    URL '${testdrive.schema-registry-url}'
  );

> CREATE CONNECTION kafka_conn
  TO KAFKA (BROKER '${testdrive.kafka-addr}', SECURITY PROTOCOL PLAINTEXT);

> CREATE SOURCE resolution_records_int2double
  IN CLUSTER ${arg.single-replica-cluster}
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-resolution-records-int2double-${testdrive.seed}')

> CREATE TABLE resolution_records_int2double_tbl FROM SOURCE resolution_records_int2double (REFERENCE "testdrive-resolution-records-int2double-${testdrive.seed}")
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE NONE

$ kafka-ingest format=avro topic=resolution-records-int2double schema=${double-col} timestamp=2
{"f1": { "f1": 123.234 }}

! SELECT * FROM resolution_records_int2double_tbl
contains:Writer schema has type `Double`, but reader schema has type `Int` for field `inner.f1`

! SELECT * FROM resolution_records_int2double_tbl
contains:failed to resolve Avro schema (id =
