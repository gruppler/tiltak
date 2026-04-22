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
