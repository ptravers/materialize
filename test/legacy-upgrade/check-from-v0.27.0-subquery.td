# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

#
# CREATE a view containing subqueries/derived tables of various types

> SHOW CREATE MATERIALIZED VIEW subquery_view;
materialize.public.subquery_view "CREATE MATERIALIZED VIEW materialize.public.subquery_view\n    IN CLUSTER quickstart\n    WITH (REFRESH = ON COMMIT)\n    AS\n        SELECT (SELECT 1)\n        FROM (SELECT 2) AS derived\n        WHERE\n            2 NOT IN ( SELECT 3 ) AND NOT EXISTS (SELECT 4) AND 5 >= ALL (SELECT 6)\n                AND\n            7 < ANY (SELECT 8);"
