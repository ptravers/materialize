// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

// BEGIN LINT CONFIG
// DO NOT EDIT. Automatically generated by bin/gen-lints.
// Have complaints about the noise? See the note in misc/python/materialize/cli/gen-lints.py first.
#![allow(clippy::style)]
#![allow(clippy::complexity)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::mutable_key_type)]
#![allow(clippy::stable_sort_primitive)]
#![allow(clippy::map_entry)]
#![allow(clippy::box_default)]
#![warn(clippy::bool_comparison)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::no_effect)]
#![warn(clippy::unnecessary_unwrap)]
#![warn(clippy::dbg_macro)]
#![warn(clippy::todo)]
#![warn(clippy::wildcard_dependencies)]
#![warn(clippy::zero_prefixed_literal)]
#![warn(clippy::borrowed_box)]
#![warn(clippy::deref_addrof)]
#![warn(clippy::double_must_use)]
#![warn(clippy::double_parens)]
#![warn(clippy::extra_unused_lifetimes)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_question_mark)]
#![warn(clippy::needless_return)]
#![warn(clippy::redundant_pattern)]
#![warn(clippy::redundant_slicing)]
#![warn(clippy::redundant_static_lifetimes)]
#![warn(clippy::single_component_path_imports)]
#![warn(clippy::unnecessary_cast)]
#![warn(clippy::useless_asref)]
#![warn(clippy::useless_conversion)]
#![warn(clippy::builtin_type_shadow)]
#![warn(clippy::duplicate_underscore_argument)]
#![warn(clippy::double_neg)]
#![warn(clippy::unnecessary_mut_passed)]
#![warn(clippy::wildcard_in_or_patterns)]
#![warn(clippy::collapsible_if)]
#![warn(clippy::collapsible_else_if)]
#![warn(clippy::crosspointer_transmute)]
#![warn(clippy::excessive_precision)]
#![warn(clippy::overflow_check_conditional)]
#![warn(clippy::as_conversions)]
#![warn(clippy::match_overlapping_arm)]
#![warn(clippy::zero_divided_by_zero)]
#![warn(clippy::must_use_unit)]
#![warn(clippy::suspicious_assignment_formatting)]
#![warn(clippy::suspicious_else_formatting)]
#![warn(clippy::suspicious_unary_op_formatting)]
#![warn(clippy::mut_mutex_lock)]
#![warn(clippy::print_literal)]
#![warn(clippy::same_item_push)]
#![warn(clippy::useless_format)]
#![warn(clippy::write_literal)]
#![warn(clippy::redundant_closure)]
#![warn(clippy::redundant_closure_call)]
#![warn(clippy::unnecessary_lazy_evaluations)]
#![warn(clippy::partialeq_ne_impl)]
#![warn(clippy::redundant_field_names)]
#![warn(clippy::transmutes_expressible_as_ptr_casts)]
#![warn(clippy::unused_async)]
#![warn(clippy::disallowed_methods)]
#![warn(clippy::disallowed_macros)]
#![warn(clippy::from_over_into)]
// END LINT CONFIG

//! AWS Kinesis utilities.

use aws_sdk_kinesis::error::{GetShardIteratorError, ListShardsError};
use aws_sdk_kinesis::model::{Shard, ShardIteratorType};
use aws_sdk_kinesis::types::SdkError;
use aws_sdk_kinesis::Client;

/// Lists the shards of the named Kinesis stream.
///
/// This function wraps the `ListShards` API call. It returns all shards in a
/// given Kinesis stream, automatically handling pagination if required.
///
/// # Errors
///
/// Any errors from the underlying `GetShardIterator` API call are surfaced
/// directly.
pub async fn list_shards(
    client: &aws_sdk_kinesis::Client,
    stream_name: &str,
) -> Result<Vec<Shard>, SdkError<ListShardsError>> {
    let mut next_token = None;
    let mut shards = Vec::new();
    loop {
        let res = client
            .list_shards()
            .set_next_token(next_token)
            .stream_name(stream_name)
            .send()
            .await?;
        shards.extend(res.shards.unwrap_or_else(Vec::new));
        if res.next_token.is_some() {
            next_token = res.next_token;
        } else {
            return Ok(shards);
        }
    }
}

/// Gets the shard IDs of the named Kinesis stream.
///
/// This function is like [`list_shards`], but
///
/// # Errors
///
/// Any errors from the underlying `GetShardIterator` API call are surfaced
/// directly.
pub async fn get_shard_ids(
    client: &Client,
    stream_name: &str,
) -> Result<impl Iterator<Item = String>, SdkError<ListShardsError>> {
    let res = list_shards(client, stream_name).await?;
    Ok(res
        .into_iter()
        .map(|s| s.shard_id.unwrap_or_else(|| "".into())))
}

/// Constructs an iterator over a Kinesis shard.
///
/// This function is a wrapper around around the `GetShardIterator` API. It
/// returns the `TRIM_HORIZON` shard iterator of a given stream and shard,
/// meaning it will return the location in the shard with the oldest data
/// record.
///
/// # Errors
///
/// Any errors from the underlying `GetShardIterator` API call are surfaced
/// directly.
pub async fn get_shard_iterator(
    client: &Client,
    stream_name: &str,
    shard_id: &str,
) -> Result<Option<String>, SdkError<GetShardIteratorError>> {
    let res = client
        .get_shard_iterator()
        .stream_name(stream_name)
        .shard_id(shard_id)
        .shard_iterator_type(ShardIteratorType::TrimHorizon)
        .send()
        .await?;
    Ok(res.shard_iterator)
}
