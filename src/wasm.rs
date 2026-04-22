use std::time::Duration;

use js_sys::Function;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::{
    ptn::{ptn_parser::parse_ptn, Game},
    position::Position,
    tei,
    tinue_search::{ProofResult, ProofTree},
};
use board_game_traits::Position as PositionTrait;

// Platform implementation for wasm using js_sys::Date for timing

struct WasmPlatform;

impl tei::Platform for WasmPlatform {
    type Instant = f64; // milliseconds from js_sys::Date::now()

    fn yield_fn() -> impl std::future::Future {
        wasm_bindgen_futures::JsFuture::from(js_sys::Promise::resolve(&JsValue::UNDEFINED))
    }

    fn current_time() -> f64 {
        js_sys::Date::now()
    }

    fn elapsed_time(start: &f64) -> Duration {
        Duration::from_millis((js_sys::Date::now() - start).max(0.0) as u64)
    }
}

/// Start the TEI engine. Returns a callback that accepts one line of TEI input at a time.
/// The `output_callback` receives one line of TEI output at a time.
#[wasm_bindgen]
pub fn start_engine(output_callback: Function) -> Function {
    let (sender, receiver) = async_channel::unbounded::<String>();

    spawn_local(async move {
        let output = move |s: &str| {
            let _ = output_callback.call1(&JsValue::NULL, &JsValue::from_str(s));
        };
        tei::tei::<_, WasmPlatform>(false, false, receiver, &output).await;
    });

    let input_closure = Closure::<dyn Fn(String)>::new(move |s: String| {
        let _ = sender.try_send(s);
    });

    let f = input_closure
        .as_ref()
        .unchecked_ref::<Function>()
        .clone();
    input_closure.forget();
    f
}

/// Parse a PTN string, annotate each move with `'` (tak) or `"` (tinue) where applicable,
/// and return the annotated PTN. Any existing tak/tinue annotations are replaced.
///
/// `tinue_nodes` controls how many proof-search iterations to run per position
/// when checking for tinue. Higher values are more accurate but slower.
/// Defaults to 500 if not provided.
#[wasm_bindgen]
pub fn annotate_ptn(ptn: &str, tinue_nodes: Option<u32>) -> Result<String, JsValue> {
    let nodes = tinue_nodes.unwrap_or(500);
    let size = extract_size(ptn)?;
    match size {
        4 => annotate_sized::<4>(ptn, nodes),
        5 => annotate_sized::<5>(ptn, nodes),
        6 => annotate_sized::<6>(ptn, nodes),
        _ => Err(JsValue::from_str(&format!("Unsupported board size: {size}"))),
    }
}

/// Check whether the position described by `tps` is "in tak" — the player who
/// just moved has an immediate winning road move available on their next turn.
/// `size` must be 4, 5, or 6.
/// Returns `true` if the position is in tak, `false` otherwise.
/// Returns an error if the TPS string cannot be parsed.
#[wasm_bindgen]
pub fn is_tak(tps: &str, size: usize) -> Result<bool, JsValue> {
    match size {
        4 => is_tak_sized::<4>(tps),
        5 => is_tak_sized::<5>(tps),
        6 => is_tak_sized::<6>(tps),
        _ => Err(JsValue::from_str(&format!("Unsupported board size: {size}"))),
    }
}

/// Check whether the position described by `tps` is tinue — the player who
/// just moved has a forced road win regardless of the opponent's play.
/// `max_nodes` limits the proof-search budget; higher values are more accurate
/// but slower. Returns `null` if the result could not be determined within
/// the node budget.
#[wasm_bindgen]
pub fn is_tinue(tps: &str, size: usize, max_nodes: u32) -> Result<Option<bool>, JsValue> {
    match size {
        4 => is_tinue_sized::<4>(tps, max_nodes),
        5 => is_tinue_sized::<5>(tps, max_nodes),
        6 => is_tinue_sized::<6>(tps, max_nodes),
        _ => Err(JsValue::from_str(&format!("Unsupported board size: {size}"))),
    }
}

fn parse_position<const S: usize>(tps: &str) -> Result<Position<S>, JsValue> {
    // Komi is irrelevant for road-win detection; parse with komi 0.
    use crate::position::Komi;
    Position::<S>::from_fen_with_komi(tps, Komi::default())
        .map_err(|e| JsValue::from_str(&e.to_string()))
}

fn is_tak_sized<const S: usize>(tps: &str) -> Result<bool, JsValue> {
    let mut position = parse_position::<S>(tps)?;
    if position.game_result().is_some() {
        return Ok(false);
    }
    position.null_move();
    Ok(position.has_winning_move())
}

fn is_tinue_sized<const S: usize>(tps: &str, max_nodes: u32) -> Result<Option<bool>, JsValue> {
    let mut position = parse_position::<S>(tps)?;
    if position.game_result().is_some() {
        return Ok(Some(false));
    }
    position.null_move();
    let mut proof_tree = ProofTree::new(position);
    for _ in 0..max_nodes {
        proof_tree.select();
        if proof_tree.result().is_some() {
            break;
        }
    }
    Ok(match proof_tree.result() {
        Some(ProofResult::Proved) => Some(true),
        Some(ProofResult::Disproved) => Some(false),
        None => None,
    })
}

fn extract_size(ptn: &str) -> Result<usize, JsValue> {
    for line in ptn.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('[') {
            if rest.to_lowercase().starts_with("size") {
                let n: usize = rest
                    .chars()
                    .filter(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .map_err(|_| JsValue::from_str("Invalid value in Size tag"))?;
                return Ok(n);
            }
        }
    }
    Err(JsValue::from_str("PTN is missing a Size tag"))
}

fn annotate_sized<const S: usize>(ptn: &str, tinue_nodes: u32) -> Result<String, JsValue> {
    let mut games: Vec<Game<Position<S>>> = parse_ptn(ptn)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    for game in &mut games {
        let mut position = game.start_position.clone();

        // First pass: play through moves and determine annotations
        let mut new_annotations: Vec<Option<&'static str>> = Vec::with_capacity(game.moves.len());
        for ptn_move in game.moves.iter() {
            position.do_move(ptn_move.mv);

            // If the game ended on this move, no threat annotation applies
            if position.game_result().is_some() {
                new_annotations.push(None);
                continue;
            }

            // Null-move: check from the perspective of the player who just moved
            let mut check_pos = position.clone();
            check_pos.null_move();

            // Check for tinue first (stronger claim) using proof-number search
            let mut proof_tree = ProofTree::new(check_pos.clone());
            for _ in 0..tinue_nodes {
                proof_tree.select();
                if proof_tree.result().is_some() {
                    break;
                }
            }

            let annotation = if proof_tree.result() == Some(ProofResult::Proved) {
                Some("\"") // tinue
            } else if check_pos.has_winning_move() {
                Some("'") // tak
            } else {
                None
            };
            new_annotations.push(annotation);
        }

        // Second pass: apply annotations (replacing any existing tak/tinue marks)
        for (ptn_move, annotation) in game.moves.iter_mut().zip(new_annotations) {
            ptn_move.annotations.retain(|a| *a != "'" && *a != "\"");
            if let Some(ann) = annotation {
                ptn_move.annotations.push(ann);
            }
        }
    }

    let mut output = String::new();
    for game in &games {
        let mut bytes = Vec::new();
        game.game_to_ptn(&mut bytes)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        output.push_str(
            &String::from_utf8(bytes)
                .map_err(|e| JsValue::from_str(&e.to_string()))?,
        );
    }
    Ok(output)
}
