// SPDX-License-Identifier: AGPL-3.0-or-later
//! M2 acceptance runner: compile an IR query against a graph and execute
//! the resulting parameterized SQL against an emitted head database.
//!
//! ```text
//! cargo run -p ros-madair-emit --example run_ir -- \
//!     <graph.json> <query.json-or-inline-json> <head.sqlite>
//! ```
//!
//! - `<graph.json>`: an Arches-style resource model export, `{"graph": [...]}`
//!   (the first graph in the array is used).
//! - `<query>`: a `ros_madair_query::Query` as JSON — either a file path or an
//!   inline JSON string (detected by a leading `{`).
//! - `<head.sqlite>`: the head DB emitted by `ros-madair-emit` — opened
//!   strictly read-only.
//!
//! On successful compilation, each compiled statement is executed and one
//! JSON object is printed with per-statement SQL, params, results, and
//! wall-clock timings. On a compilation error, the typed `QueryError` is
//! printed as JSON (`{"error": {...}}`) and the process exits 1 — this is
//! the machine-repairable channel, demonstrated in the acceptance set with
//! a string-path (`not_head_indexed`) query.

use std::process::ExitCode;
use std::time::Instant;

use alizarin_core::graph::StaticGraph;
use ros_madair_query::{self as query_ir, Measure, Param, Query};
use rusqlite::{types::Value as SqlValue, Connection, OpenFlags};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: run_ir <graph.json> <query.json|inline-json> <head.sqlite>");
        return ExitCode::from(2);
    }
    match run(&args[1], &args[2], &args[3]) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("run_ir failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(graph_path: &str, query_arg: &str, head_path: &str) -> Result<ExitCode, Box<dyn std::error::Error>> {
    // Load graph: {"graph": [...]} export; first graph in the array.
    let graph_json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(graph_path)?)?;
    let graph_value = graph_json
        .get("graph")
        .and_then(|g| g.get(0))
        .ok_or("graph file has no graph[0]")?
        .clone();
    let mut graph: StaticGraph = serde_json::from_value(graph_value)?;
    graph.build_indices();

    // Query: inline JSON (leading '{') or a file path.
    let query_text = if query_arg.trim_start().starts_with('{') {
        query_arg.to_string()
    } else {
        std::fs::read_to_string(query_arg)?
    };
    let query: Query = serde_json::from_str(&query_text)?;

    // Compile through the emitter's default registry (the CLM reference
    // handler is registered there), so `reference` fields are queryable via
    // the IR exactly as core datatypes are — core itself names no `reference`.
    let registry = ros_madair_emit::default_registry();
    let statements = match query_ir::compile_with_registry(&query, &graph, Some(&registry)) {
        Ok(stmts) => stmts,
        Err(err) => {
            let out = serde_json::json!({ "error": err, "message": err.to_string() });
            println!("{}", serde_json::to_string_pretty(&out)?);
            return Ok(ExitCode::FAILURE);
        }
    };

    // Head DB is a static artifact: read-only, no create.
    let conn = Connection::open_with_flags(
        head_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;

    let mut results = Vec::with_capacity(statements.len());
    for stmt in &statements {
        let bound: Vec<SqlValue> = stmt
            .params
            .iter()
            .map(|p| match p {
                Param::Text(s) => SqlValue::Text(s.clone()),
                Param::Int(i) => SqlValue::Integer(*i),
            })
            .collect();
        let started = Instant::now();
        let result = match stmt.measure {
            Measure::CountRecords => {
                let count: i64 = conn.query_row(
                    &stmt.sql,
                    rusqlite::params_from_iter(bound.iter()),
                    |row| row.get(0),
                )?;
                serde_json::json!(count)
            }
            Measure::SelectIds => {
                let mut prepared = conn.prepare(&stmt.sql)?;
                let ids: Vec<String> = prepared
                    .query_map(rusqlite::params_from_iter(bound.iter()), |row| row.get(0))?
                    .collect::<Result<_, _>>()?;
                serde_json::json!(ids)
            }
        };
        let elapsed = started.elapsed();
        results.push(serde_json::json!({
            "measure": stmt.measure,
            "sql": stmt.sql,
            "params": stmt.params,
            "coarse": stmt.coarse,
            "elapsed_us": elapsed.as_micros() as u64,
            "result": result,
        }));
    }

    let out = serde_json::json!({ "model": query.model, "statements": results });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(ExitCode::SUCCESS)
}
