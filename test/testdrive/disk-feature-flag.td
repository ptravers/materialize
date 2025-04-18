# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ postgres-execute connection=postgres://mz_system:materialize@${testdrive.materialize-internal-sql-addr}
ALTER SYSTEM SET enable_disk_cluster_replicas = false
ALTER SYSTEM SET disk_cluster_replicas_default = false

! CREATE CLUSTER no SIZE = '1-no-disk', REPLICATION FACTOR 0, DISK;
exact:`WITH (DISK)` for cluster replicas is not available

> CREATE CLUSTER no SIZE = '1-no-disk', REPLICATION FACTOR 0;

! ALTER CLUSTER no SET (REPLICATION FACTOR 1, DISK);
exact:`WITH (DISK)` for cluster replicas is not available

> DROP CLUSTER no;

# Test that if `enable_disk_cluster_replicas` is on, we can use `WITH(DISK)`.
$ postgres-execute connection=postgres://mz_system:materialize@${testdrive.materialize-internal-sql-addr}
ALTER SYSTEM SET enable_disk_cluster_replicas = true

> DROP CLUSTER IF EXISTS c;

# Can set unmanaged cluster replica options directly, mixing and matching disk
> CREATE CLUSTER c REPLICAS (r1 (SIZE '1-no-disk', DISK), r2 (SIZE '1-no-disk'))

> SELECT r.name, r.size, r.disk FROM mz_catalog.mz_clusters c, mz_catalog.mz_cluster_replicas r WHERE c.name = 'c' AND c.id = r.cluster_id;
r1 1-no-disk true
r2 1-no-disk false

> DROP CLUSTER c;

# Can set on managed clusters
> CREATE CLUSTER c SIZE '1-no-disk', REPLICATION FACTOR = 2, DISK;

> SELECT r.name, r.size, r.disk FROM mz_catalog.mz_clusters c, mz_catalog.mz_cluster_replicas r WHERE c.name = 'c' AND c.id = r.cluster_id;
r1 1-no-disk true
r2 1-no-disk true

> DROP CLUSTER c;

# Can toggle whether `DISK` defaults true or false
$ postgres-execute connection=postgres://mz_system:materialize@${testdrive.materialize-internal-sql-addr}
ALTER SYSTEM SET disk_cluster_replicas_default = true

> CREATE CLUSTER c REPLICAS (r1 (SIZE '1-no-disk', DISK), r2 (SIZE '1-no-disk'))

> SELECT r.name, r.size, r.disk FROM mz_catalog.mz_clusters c, mz_catalog.mz_cluster_replicas r WHERE c.name = 'c' AND c.id = r.cluster_id;
r1 1-no-disk true
r2 1-no-disk true

> DROP CLUSTER c;

$ postgres-execute connection=postgres://mz_system:materialize@${testdrive.materialize-internal-sql-addr}
ALTER SYSTEM RESET disk_cluster_replicas_default

# Can disable whether disk-backed replicas are allowed
$ postgres-execute connection=postgres://mz_system:materialize@${testdrive.materialize-internal-sql-addr}
ALTER SYSTEM SET enable_disk_cluster_replicas = false

! CREATE CLUSTER c REPLICAS (dff_3 (size '1-no-disk', disk))
contains:`WITH (DISK)` for cluster replicas is not available

# Cannot set DISK on unmanaged clusters (the option is per replica)
! CREATE CLUSTER c REPLICAS (dff_3 (size '1-no-disk')), DISK;
contains:DISK not supported for unmanaged clusters

# The following test that we don't crash envd with bad parameters, and instead just fallback
# to safe parameters.
$ postgres-execute connection=postgres://mz_system:materialize@${testdrive.materialize-internal-sql-addr}
ALTER SYSTEM SET upsert_rocksdb_universal_compaction_ratio = 50;
$ postgres-execute connection=postgres://mz_system:materialize@${testdrive.materialize-internal-sql-addr}
ALTER SYSTEM RESET upsert_rocksdb_universal_compaction_ratio;
$ postgres-execute connection=postgres://mz_system:materialize@${testdrive.materialize-internal-sql-addr}
ALTER SYSTEM SET upsert_rocksdb_parallelism = -1;
$ postgres-execute connection=postgres://mz_system:materialize@${testdrive.materialize-internal-sql-addr}
ALTER SYSTEM RESET upsert_rocksdb_parallelism
