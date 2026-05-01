#![allow(clippy::uninlined_format_args)]

use board_game_traits::{Color, Position as PositionTrait};
use pgn_traits::PgnPosition;
use std::any::Any;
use std::io::{BufRead, BufReader};
use std::str::FromStr;
use std::sync::atomic::{self, AtomicBool};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use std::{env, io};

use tiltak::position::{Komi, Move, Position};
use tiltak::search::{MctsSetting, MonteCarloTree};

/// Tracks the current "search position" as a root + a list of applied moves so
/// that a new `position` command can be detected as a descendant of the
/// previous one, allowing us to reroot the existing search tree instead of
/// rebuilding from scratch.
#[derive(Clone)]
struct SearchPosition<const S: usize> {
    root_position: Position<S>,
    moves: Vec<Move<S>>,
}

impl<const S: usize> SearchPosition<S> {
    fn position(&self) -> Position<S> {
        let mut position = self.root_position.clone();
        for mv in &self.moves {
            position.do_move(*mv);
        }
        position
    }

    /// Returns the moves needed to reach `new_position` from `self` if
    /// `new_position` is a descendant; otherwise `None`.
    fn move_difference<'a>(
        &self,
        new_position: &'a SearchPosition<S>,
    ) -> Option<&'a [Move<S>]> {
        if self.root_position != new_position.root_position {
            return None;
        }
        if new_position.moves.len() < self.moves.len() {
            return None;
        }
        for (a, b) in self.moves.iter().zip(new_position.moves.iter()) {
            if a != b {
                return None;
            }
        }
        Some(&new_position.moves[self.moves.len()..])
    }
}

pub fn main() {
    let is_slatebot = env::args().any(|arg| arg == "--slatebot");
    let is_cobblebot = env::args().any(|arg| arg == "--cobblebot");

    loop {
        let mut input = String::new();
        io::stdin().read_line(&mut input).unwrap();
        if input.trim() == "tei" {
            break;
        }
    }

    println!("id name Tiltak");
    println!("id author Morten Lohne");
    println!("option name HalfKomi type spin default 0 min -10 max 10");
    println!("option name MultiPV type spin default 1 min 1 max 8");
    println!("teiok");

    // Size-erased state — concrete types depend on the current `size`.
    // `position` / `last_searched` hold `SearchPosition<S>`.
    // `search_tree` holds `MonteCarloTree<S>`.
    // `calculating_handle` returns `(Box<MonteCarloTree<S>>, Duration)` size-erased
    // via `Box<dyn Any + Send>`; the `Duration` is the time actually spent in
    // this `go` and is added to `cumulative_search_time` on join.
    let mut position: Option<Box<dyn Any>> = None;
    let mut last_searched: Option<Box<dyn Any>> = None;
    let mut search_tree: Option<Box<dyn Any + Send>> = None;
    let mut size: Option<usize> = None;
    let mut komi = Komi::default();
    let mut multi_pv: usize = 1;
    let mut cumulative_search_time = Duration::ZERO;
    let mut calculating_handle: Option<JoinHandle<(Option<Box<dyn Any + Send>>, Duration)>> = None;
    let should_stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    for line in BufReader::new(io::stdin()).lines().map(Result::unwrap) {
        let mut words = line.split_whitespace();
        match words.next().unwrap() {
            "quit" => {
                should_stop.store(true, atomic::Ordering::Relaxed);
                if let Some(handle) = calculating_handle.take() {
                    handle.join().unwrap();
                }
                break;
            }
            "stop" => {
                should_stop.store(true, atomic::Ordering::Relaxed);
                drain_worker(
                    &mut calculating_handle,
                    &mut search_tree,
                    &mut cumulative_search_time,
                );
                should_stop.store(false, atomic::Ordering::Relaxed);
            }
            "isready" => println!("readyok"),
            "setoption" => {
                let header = [
                    words.next().unwrap_or_default(),
                    words.next().unwrap_or_default(),
                ];
                if header != ["name", "HalfKomi"] && header != ["name", "MultiPV"] {
                    panic!("Invalid setoption string \"{}\"", line);
                }
                let option_name = header[1];
                if words.next() != Some("value") {
                    panic!("Invalid setoption string \"{}\"", line);
                }
                let value = words.next().unwrap_or_default();
                match option_name {
                    "HalfKomi" => {
                        if let Some(k) =
                            value.parse::<i8>().ok().and_then(Komi::from_half_komi)
                        {
                            komi = k;
                        } else {
                            panic!("Invalid komi setting \"{}\"", line);
                        }
                    }
                    "MultiPV" => {
                        if let Some(n) = value.parse::<usize>().ok().filter(|&n| (1..=8).contains(&n))
                        {
                            multi_pv = n;
                        } else {
                            panic!("Invalid MultiPV setting \"{}\"", line);
                        }
                    }
                    _ => unreachable!(),
                }
            }
            "teinewgame" => {
                drain_worker(
                    &mut calculating_handle,
                    &mut search_tree,
                    &mut cumulative_search_time,
                );
                let size_string = words.next();
                size = size_string.and_then(|s| usize::from_str(s).ok());
                position = None;
                last_searched = None;
                search_tree = None;
                cumulative_search_time = Duration::ZERO;

                match size {
                    Some(4) | Some(5) | Some(6) | Some(7) => (),
                    _ => panic!("Error: Unsupported size {}", size.unwrap_or_default()),
                }
            }
            "position" => {
                drain_worker(
                    &mut calculating_handle,
                    &mut search_tree,
                    &mut cumulative_search_time,
                );
                position = match size {
                    None => panic!("Received position without receiving teinewgame string"),
                    Some(4) => Some(Box::new(parse_position_string::<4>(&line, komi))),
                    Some(5) => Some(Box::new(parse_position_string::<5>(&line, komi))),
                    Some(6) => Some(Box::new(parse_position_string::<6>(&line, komi))),
                    Some(7) => Some(Box::new(parse_position_string::<7>(&line, komi))),
                    Some(s) => panic!("Unsupported size {}", s),
                }
            }
            "go" => {
                drain_worker(
                    &mut calculating_handle,
                    &mut search_tree,
                    &mut cumulative_search_time,
                );
                let should_stop_clone = should_stop.clone();
                calculating_handle = match size {
                    Some(4) => Some(spawn_go::<4>(
                        line.clone(),
                        &mut position,
                        &mut last_searched,
                        &mut search_tree,
                        &mut cumulative_search_time,
                        is_slatebot,
                        is_cobblebot,
                        should_stop_clone,
                        multi_pv,
                    )),
                    Some(5) => Some(spawn_go::<5>(
                        line.clone(),
                        &mut position,
                        &mut last_searched,
                        &mut search_tree,
                        &mut cumulative_search_time,
                        is_slatebot,
                        is_cobblebot,
                        should_stop_clone,
                        multi_pv,
                    )),
                    Some(6) => Some(spawn_go::<6>(
                        line.clone(),
                        &mut position,
                        &mut last_searched,
                        &mut search_tree,
                        &mut cumulative_search_time,
                        is_slatebot,
                        is_cobblebot,
                        should_stop_clone,
                        multi_pv,
                    )),
                    Some(7) => Some(spawn_go::<7>(
                        line.clone(),
                        &mut position,
                        &mut last_searched,
                        &mut search_tree,
                        &mut cumulative_search_time,
                        is_slatebot,
                        is_cobblebot,
                        should_stop_clone,
                        multi_pv,
                    )),
                    Some(s) => panic!("Error: Unsupported size {}", s),
                    None => panic!("Error: Received go without receiving teinewgame string"),
                };
            }
            s => panic!("Unknown command \"{}\"", s),
        }
    }
}

fn build_mcts_settings<const S: usize>(is_slatebot: bool, is_cobblebot: bool) -> MctsSetting<S> {
    let mut s = if is_slatebot {
        MctsSetting::default()
            .add_rollout_depth(200)
            .add_rollout_temperature(0.2)
    } else if is_cobblebot {
        MctsSetting::default()
            .add_rollout_depth(200)
            .add_rollout_temperature(0.2)
            .add_dirichlet(0.25)
    } else {
        MctsSetting::default()
    };
    // Optional override, primarily for testing arena-exhaustion handling.
    // Value is megabytes; arena slots are 16 bytes each.
    if let Ok(mb_str) = env::var("TILTAK_ARENA_SIZE_MB") {
        if let Ok(mb) = mb_str.parse::<u32>() {
            let slots = mb.saturating_mul(1024 * 1024 / 16);
            s = s.arena_size(slots);
        }
    }
    s
}

/// Drain the worker if running, recovering the tree and the time it actually
/// spent searching (used to advance `cumulative_search_time` monotonically
/// across reused trees).
fn drain_worker(
    handle: &mut Option<JoinHandle<(Option<Box<dyn Any + Send>>, Duration)>>,
    search_tree: &mut Option<Box<dyn Any + Send>>,
    cumulative: &mut Duration,
) {
    if let Some(h) = handle.take() {
        let (tree_box, elapsed) = h.join().unwrap();
        *search_tree = tree_box;
        *cumulative += elapsed;
    }
}

/// Build / reroot the search tree for the next `go` command and spawn the
/// worker thread. Updates `last_searched` and resets `cumulative_search_time`
/// when the tree had to be rebuilt.
#[allow(clippy::too_many_arguments)]
fn spawn_go<const S: usize>(
    line: String,
    position: &mut Option<Box<dyn Any>>,
    last_searched: &mut Option<Box<dyn Any>>,
    search_tree: &mut Option<Box<dyn Any + Send>>,
    cumulative_search_time: &mut Duration,
    is_slatebot: bool,
    is_cobblebot: bool,
    should_stop: Arc<AtomicBool>,
    multi_pv: usize,
) -> JoinHandle<(Option<Box<dyn Any + Send>>, Duration)> {
    let mcts_settings: MctsSetting<S> = build_mcts_settings(is_slatebot, is_cobblebot);
    let cur_pos: SearchPosition<S> = position
        .as_ref()
        .and_then(|p| p.downcast_ref::<SearchPosition<S>>())
        .expect("position must be set before go")
        .clone();

    let prev_tree = search_tree
        .take()
        .and_then(|b| b.downcast::<MonteCarloTree<S>>().ok())
        .map(|b| *b);
    let prev_pos = last_searched
        .take()
        .and_then(|b| b.downcast::<SearchPosition<S>>().ok())
        .map(|b| *b);

    let (tree, reused) = match (prev_tree, prev_pos) {
        (Some(t), Some(prev)) => match prev.move_difference(&cur_pos) {
            Some(diff) => match t.reroot(diff) {
                Some(t) => (t, true),
                None => (
                    MonteCarloTree::new(cur_pos.position(), mcts_settings.clone()),
                    false,
                ),
            },
            None => (
                MonteCarloTree::new(cur_pos.position(), mcts_settings.clone()),
                false,
            ),
        },
        _ => (
            MonteCarloTree::new(cur_pos.position(), mcts_settings.clone()),
            false,
        ),
    };

    if !reused {
        *cumulative_search_time = Duration::ZERO;
    }

    *last_searched = Some(Box::new(cur_pos.clone()));

    let time_offset = *cumulative_search_time;
    let position_for_worker = cur_pos.position();

    thread::spawn(move || {
        let go_start = Instant::now();
        let tree = run_go::<S>(
            &line,
            position_for_worker,
            tree,
            should_stop,
            multi_pv,
            time_offset,
        );
        let elapsed = go_start.elapsed();
        let boxed: Option<Box<dyn Any + Send>> =
            tree.map(|t| Box::new(t) as Box<dyn Any + Send>);
        (boxed, elapsed)
    })
}

fn parse_position_string<const S: usize>(line: &str, komi: Komi) -> SearchPosition<S> {
    let mut words_iter = line.split_whitespace();
    words_iter.next(); // position
    let root_position = match words_iter.next() {
        Some("startpos") => Position::start_position_with_komi(komi),
        Some("tps") => {
            let tps: String = (&mut words_iter).take(3).collect::<Vec<_>>().join(" ");
            <Position<S>>::from_fen_with_komi(&tps, komi).unwrap()
        }
        _ => panic!("Expected \"startpos\" or \"tps\" to specify position."),
    };

    let mut moves: Vec<Move<S>> = Vec::new();
    match words_iter.next() {
        Some("moves") => {
            // Replay moves on a scratch position so we can parse SAN against the running state.
            let mut scratch = root_position.clone();
            for move_string in words_iter {
                let mv = scratch.move_from_san(move_string).unwrap();
                scratch.do_move(mv);
                moves.push(mv);
            }
        }
        Some(s) => panic!("Expected \"moves\" in \"{}\", got \"{}\".", line, s),
        None => (),
    }
    SearchPosition {
        root_position,
        moves,
    }
}

fn print_search_info<const S: usize>(
    tree: &MonteCarloTree<S>,
    position: &Position<S>,
    start_time: Instant,
    time_offset: Duration,
    visits_at_start: u32,
    multi_pv: usize,
) {
    let elapsed = start_time.elapsed().max(Duration::from_micros(1));
    let reported_time_ms = (time_offset + elapsed).as_millis();
    let total_visits = tree.visits();
    let nodes_this_go = total_visits.saturating_sub(visits_at_start);
    let nps = nodes_this_go as f32 / elapsed.as_secs_f32();
    let depth = ((total_visits as f64 / 10.0).log2()) as u64;

    if multi_pv > 1 {
        for (index, edge) in tree.best_moves(multi_pv).iter().enumerate() {
            let score = 1.0 - edge.mean_action_value;
            let wdl = [score, 0.0, 1.0 - score];
            let pv_moves: Vec<_> = std::iter::once(edge.mv)
                .chain(tree.pv_from_edge(edge))
                .collect();
            println!(
                "info multipv {} depth {} seldepth {} nodes {} score cp {} wdl {} {} {} time {} nps {:.0} pv {}",
                index + 1,
                depth,
                pv_moves.len(),
                total_visits,
                (score * 200.0 - 100.0) as i64,
                (wdl[0] * 1000.0).round() as i64,
                (wdl[1] * 1000.0).round() as i64,
                (wdl[2] * 1000.0).round() as i64,
                reported_time_ms,
                nps,
                pv_moves
                    .iter()
                    .map(|mv| position.move_to_san(mv))
                    .collect::<Vec<String>>()
                    .join(" ")
            );
        }
    } else {
        let best_score = tree.best_move().unwrap().1;
        let wdl = [best_score, 0.0, 1.0 - best_score];
        let pv: Vec<_> = tree.pv().collect();
        println!(
            "info depth {} seldepth {} nodes {} score cp {} wdl {} {} {} time {} nps {:.0} pv {}",
            depth,
            pv.len(),
            total_visits,
            (best_score * 200.0 - 100.0) as i64,
            (wdl[0] * 1000.0).round() as i64,
            (wdl[1] * 1000.0).round() as i64,
            (wdl[2] * 1000.0).round() as i64,
            reported_time_ms,
            nps,
            pv.iter()
                .map(|mv| position.move_to_san(mv))
                .collect::<Vec<String>>()
                .join(" ")
        );
    }
}

fn run_go<const S: usize>(
    line: &str,
    position: Position<S>,
    mut tree: MonteCarloTree<S>,
    should_stop: Arc<AtomicBool>,
    multi_pv: usize,
    time_offset: Duration,
) -> Option<MonteCarloTree<S>> {
    let mut words = line.split_whitespace();
    words.next(); // go

    let visits_at_start = tree.visits();
    let mut tree_exhausted = false;

    match words.next() {
        Some(word @ "movetime") | Some(word @ "infinite") => {
            let movetime = if word == "movetime" {
                Duration::from_millis(u64::from_str(words.next().unwrap()).unwrap())
            } else {
                Duration::MAX
            };
            let start_time = Instant::now();

            for i in 0.. {
                let nodes_to_search = (200.0 * f64::powf(1.26, i as f64)) as u64;
                let mut oom = false;
                for _ in 0..nodes_to_search {
                    if should_stop.load(atomic::Ordering::Relaxed) {
                        break;
                    }
                    if let Err(err) = tree.select() {
                        eprintln!("Warning: {err}");
                        oom = true;
                        tree_exhausted = true;
                        break;
                    }
                }
                let (best_move, _) = tree.best_move().unwrap();
                print_search_info(
                    &tree,
                    &position,
                    start_time,
                    time_offset,
                    visits_at_start,
                    multi_pv,
                );
                if oom
                    || should_stop.load(atomic::Ordering::Relaxed)
                    || start_time.elapsed().as_secs_f64() > movetime.as_secs_f64() * 0.7
                {
                    println!("bestmove {}", position.move_to_san(&best_move));
                    break;
                }
            }
        }
        Some("wtime") | Some("btime") | Some("winc") | Some("binc") => {
            let parse_time = |s: Option<&str>| {
                Duration::from_millis(
                    s.and_then(|w| w.parse().ok())
                        .unwrap_or_else(|| panic!("Incorrect go command {}", line)),
                )
            };
            let mut words = line.split_whitespace().skip(1).peekable();
            let mut white_time = Duration::default();
            let mut white_inc = Duration::default();
            let mut black_time = Duration::default();
            let mut black_inc = Duration::default();

            while let Some(word) = words.next() {
                match word {
                    "wtime" => white_time = parse_time(words.next()),
                    "winc" => white_inc = parse_time(words.next()),
                    "btime" => black_time = parse_time(words.next()),
                    "binc" => black_inc = parse_time(words.next()),
                    _ => (),
                }
            }

            let max_time = match position.side_to_move() {
                Color::White => white_time / 5 + white_inc / 2,
                Color::Black => black_time / 5 + black_inc / 2,
            };

            let start_time = Instant::now();

            if tree
                .search_for_time(max_time, |tree| {
                    print_search_info(
                        tree,
                        &position,
                        start_time,
                        time_offset,
                        visits_at_start,
                        multi_pv,
                    );
                })
                .is_err()
            {
                tree_exhausted = true;
            }
            let best_move = tree.best_move().unwrap().0;

            println!("bestmove {}", position.move_to_san(&best_move));
        }
        Some(_) | None => {
            panic!("Invalid go command \"{}\"", line);
        }
    }

    if tree_exhausted {
        None
    } else {
        Some(tree)
    }
}

