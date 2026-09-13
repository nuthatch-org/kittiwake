//! The QoS quality score: how well an indexer serves the queries a gateway routes to it.
//!
//! Ported from Lodestar's `src/lib/qos-score.ts` and `src/lib/qos-aggregate.ts`, removed with the
//! gateway key in `cb61cea`, with every constant unchanged so a grade computed here reads the same
//! as one the dashboard published before. `tests/ts_equivalence.rs` holds the port to the
//! TypeScript on generated inputs. Pure functions only; the reads and routes live elsewhere.
//!
//! Not to be confused with `kittiwake-score`, the eleven-dimension composite, which consumes the
//! served gap from here as one of its inputs.

pub mod aggregate;
pub mod nest;
pub mod score;
