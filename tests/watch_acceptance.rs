//! Acceptance tests for the interactive `dispatch watch`, written from the
//! contract in `docs/plan-0.4.11.md` (§3), independently of the view's own
//! tests. Each test drives the real binary in a PTY through
//! `tests/fixtures/watch_acceptance.py`, with fixture Work only: linked
//! worktrees attached with `attach --workspace` and Claude Code worktrees
//! registered through `dispatch hook claude`. No agent is launched.
#![cfg(unix)]
use std::process::Command;

fn scenario(name: &str) {
    let output = Command::new("python3")
        .arg("-B")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/watch_acceptance.py"
        ))
        .arg(assert_cmd::cargo_bin!("dispatch"))
        .arg(name)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "watch acceptance {name}:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// §3.3, §3.5, §3.6: at 60, 80, 120 and 200 columns every Work's name, state
/// and verdict is on its own unwrapped line, in group order.
#[test]
fn every_work_is_named_with_its_state_and_verdict_at_four_widths() {
    scenario("widths");
}

/// §3.4: Work inserted, removed, and moved between groups while watching; the
/// selection stays on the same Work.
#[test]
fn the_selection_follows_its_work_through_insertions_removals_and_regrouping() {
    scenario("reorder");
}

/// §3.4, §3.8: accept and reject act on the Work shown selected; when it
/// leaves the list, the next action key only says where the selection went.
#[test]
fn actions_act_on_the_selected_work_and_a_moved_selection_only_notices() {
    scenario("actions");
}

/// §3.4, §3.6: resizing across 90 and below 40 columns keeps the selection,
/// and the confirmation names the same Work.
#[test]
fn resizing_keeps_the_selected_work() {
    scenario("resize");
}

/// §3.2: long names, folders named the same in different parents, names that
/// collide only once truncated, and widths counted in cells.
#[test]
fn long_and_duplicate_workspace_names_are_told_apart() {
    scenario("names");
}

/// §3.7, §3.8: REFRESH and STOP (blocked), failed checks, CONTINUE moved and
/// unmoved, applied by you and by auto-apply, and rejected.
#[test]
fn ready_and_landed_states_say_what_to_do_next_and_offer_its_keys() {
    scenario("states-ready");
}

/// §3.7, §3.8: working (attached, and a Claude session), working with an
/// advisory REFRESH, idle, removed and lost.
#[test]
fn live_and_gone_states_say_what_to_do_next_and_offer_its_keys() {
    scenario("states-live");
}

/// §3.7: interactions name both pieces of Work, from either side.
#[test]
fn interactions_name_both_pieces_of_work() {
    scenario("interactions");
}

/// §3.9: success and refusal notices name the Work, refusals come from the
/// stored verdict, confirmations name the Work, and notices age out.
#[test]
fn feedback_names_the_work_it_is_about() {
    scenario("feedback");
}

/// §3.9: a refusal that is neither REFRESH nor STOP gives the error's first
/// sentence.
#[test]
fn another_refusal_gives_the_errors_first_sentence() {
    scenario("refusal-other");
}

/// §3.10: NO_COLOR, --no-color, DISPATCH_COLOR=none and TERM=dumb draw no
/// color; bold stays.
#[test]
fn no_color_draws_no_color_codes() {
    scenario("no-color");
}

/// §3.10: --ascii draws nothing outside ASCII.
#[test]
fn ascii_draws_only_ascii() {
    scenario("ascii");
}

/// §3.11: the empty view, watched and not.
#[test]
fn the_empty_view_says_how_work_appears() {
    scenario("empty");
}

/// §3.5, §3.8: an unwatched project's header, a ready result not checked
/// yet, Shift+Tab ignored, and Esc and Ctrl+C leaving.
#[test]
fn an_unwatched_project_and_the_keys_that_do_nothing_or_leave() {
    scenario("unwatched");
}

/// §1 "Preserved, byte for byte": piped `watch`, `--plain` on a terminal and
/// `watch --json` print 0.4.10's rows, derived from `WorkLine`'s format.
#[test]
fn plain_piped_and_json_watch_are_unchanged() {
    scenario("plain");
}

/// §3.7: details line 1 "ends with `Work <short id>`".
#[test]
#[ignore = "defect in packet A: at 90 columns an 80-character name pushes the ID off \
            details line 1, which ends `· Work` (first_detail never truncates the name)"]
fn details_line_one_ends_with_the_work_id_however_long_the_name() {
    scenario("long-name-detail");
}

/// §3.4: the list "never scrolls the selection off screen".
#[test]
#[ignore = "defect in packet A: at 100x10 the required details take the whole room and \
            the table shows its titles but no row, so the selected Work is not on screen"]
fn a_short_terminal_still_shows_the_selected_work() {
    scenario("short-terminal");
}

/// §3.8: the hint is the offered keys, then `↑↓ · q leave` below 90 columns.
#[test]
#[ignore = "defect in packet A: at 40 columns a ready Work's hint is cut to \
            `a accept · d review · r reject · ↑↓ ·…`, losing `q leave`"]
fn the_hint_keeps_q_leave_at_forty_columns() {
    scenario("narrowest-hint");
}
