// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Representation of and serialization for PostgreSQL datums.
//!
//! This crate exports a [`Value`] type that maps directly to a PostgreSQL
//! datum. These values can be serialized using either the text or binary
//! encoding format; see the [`mz_pgwire_common::Format`] type for details.
//!
//! `Value`s are easily converted to and from [`mz_repr::Datum`]s. See, for
//! example, the [`values_from_row`] function.

#![warn(clippy::as_conversions)]
#![warn(missing_docs)]

mod types;
mod value;

pub mod oid;

pub use types::{
    ANYCOMPATIBLELIST, ANYCOMPATIBLEMAP, LIST, MAP, Type, TypeConversionError, TypeFromOidError,
};
pub use value::interval::Interval;
pub use value::jsonb::Jsonb;
pub use value::numeric::Numeric;
pub use value::record::Record;
pub use value::unsigned::{UInt2, UInt4, UInt8};
pub use value::{Value, values_from_row};
