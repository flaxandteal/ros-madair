// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Re-exports query planning types and functions from `ros-madair-core`.

pub use ros_madair_core::query::{
    plan_from_patterns, execute_patterns,
    FetchPlan, PageFetchSpec, PatternTerm, SummaryResult, TriplePattern,
};
