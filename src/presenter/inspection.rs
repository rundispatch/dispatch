//! User-invoked inspection. This owns only view selection, never delivery authority.
use super::*;
use crate::reviewer::{ReviewBundle, ReviewerPreference};

const PAGE_BYTES: usize = 32 * 1024;
fn tail_clip(text: &str, width: u16) -> String {
    if UnicodeWidthStr::width(text) <= usize::from(width) {
        return text.to_owned();
    }
    let mut tail = Vec::new();
    let mut cells = 0;
    for g in text.graphemes(true).rev() {
        let n = UnicodeWidthStr::width(g);
        if cells + n + 3 > usize::from(width) {
            break;
        }
        tail.push(g);
        cells += n;
    }
    format!(
        "{}{}",
        "...".chars().take(width.into()).collect::<String>(),
        tail.into_iter().rev().collect::<String>()
    )
}
fn path_label(path: &std::path::Path) -> String {
    path.to_string_lossy()
        .chars()
        .flat_map(|c| {
            if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

#[derive(Default)]
pub(super) struct ReviewView {
    selected: usize,
    filter: String,
    file: bool,
    offset: u64,
    row: usize,
    column: u16,
}

fn compact_patch(text: &str) -> String {
    sanitize(text)
        .lines()
        .filter(|line| {
            !line.starts_with("diff --git ")
                && !line.starts_with("index ")
                && !line.starts_with("--- ")
                && !line.starts_with("+++ ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn tiny_preview(bundle: &ReviewBundle) -> Result<String> {
    if bundle.entries.len() != 1 {
        return Ok(String::new());
    }
    let entry = &bundle.entries[0];
    if entry.binary
        || entry.bytes > 4096
        || entry.hunks.len() > 1
        || entry.additions.unwrap_or(0) + entry.deletions.unwrap_or(0) > 4
    {
        return Ok(String::new());
    }
    let preview = compact_patch(&bundle.preview(0, 0, 4096)?.text);
    let changes = preview
        .lines()
        .filter(|line| line.starts_with(['+', '-']))
        .take(4)
        .collect::<Vec<_>>()
        .join("\n");
    if changes.is_empty() {
        return Ok(String::new());
    }
    Ok(format!(
        "Preview · {}\n{}",
        path_label(&entry.path),
        changes
    ))
}

impl Ui {
    /// Drain Crossterm's parsed queue AND the OS queue at ownership boundaries.
    /// Never flush while a cursor-position query is in flight.
    pub(super) async fn input_boundary(&mut self) -> Result<()> {
        self.discard_work_input()?;
        if self.options.plain {
            return Ok(());
        }
        #[cfg(unix)]
        unsafe {
            libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
        }
        let start = Instant::now();
        let mut quiet = Instant::now();
        while quiet.elapsed() < Duration::from_millis(60)
            && start.elapsed() < Duration::from_millis(300)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if event::poll(Duration::ZERO)? {
                self.discard_work_input()?;
                quiet = Instant::now();
            }
        }
        Ok(())
    }

    pub(super) async fn review_action(
        &mut self,
        run: &RunRecord,
        preview: &str,
        notice: &str,
    ) -> Result<Input> {
        if self.closed {
            return Ok(Input::Eof);
        }
        // Evaluated once per review prompt, not per redraw; display only.
        let run = &crate::coherence::with_live_validity(run);
        let mut action = String::new();
        let mut summary = projection(
            run,
            None,
            self.width().saturating_sub(3),
            self.options.ascii,
        );
        summary.push_str(&format!("\n{}", self.launch_line));
        let actions = "[Enter/d] Review changes   [e] Open in editor\n[a] Accept & apply   [r] Reject\n[aa] Accept & apply, then auto-apply the next results\n[n] Leave pending   [i] Details";
        if self.options.plain {
            return self
                .command_prompt(&format!("{summary}\n{preview}\n{notice}\n{actions}"))
                .await;
        }
        loop {
            let width = self.width().saturating_sub(3);
            let body = self.display_text(&format!(
                "{}\n{}",
                projection(run, None, width, self.options.ascii),
                self.launch_line
            ));
            let preview = self.display_text(preview);
            let notice = self.display_text(notice);
            let mode_hint = self.display_text(&self.mode_hint());
            let auto_apply = self.auto_apply;
            let screen = self.screen.as_mut().context("terminal is unavailable")?;
            screen.terminal.draw(|frame| {
                let area = content_area(frame.area());
                let mut lines = styled_body(&body, &self.palette).lines;
                // Summary stays compact. The full graph belongs to work, not the decision.
                lines.retain(|line| {
                    let t = line.to_string();
                    !t.contains(" ━━ ") && !t.contains(" == ")
                });
                if !preview.is_empty() && area.width >= 45 && area.height >= 12 {
                    lines.push(Line::default());
                    lines.extend(diff_lines(&preview, &self.palette));
                }
                if !notice.is_empty() {
                    lines.push(Line::styled(
                        clip(&notice, area.width),
                        self.palette.warning,
                    ));
                }
                let controls = vec![
                    Line::styled(
                        mode_hint,
                        if auto_apply {
                            self.palette.warning
                        } else {
                            self.palette.secondary
                        },
                    ),
                    Line::styled(
                        "[Enter/d] Review changes   [e] Open in editor",
                        self.palette.focus,
                    ),
                    Line::styled("[a] Accept & apply   [r] Reject", self.palette.foreground),
                    Line::styled(
                        "[aa] Accept & apply, then auto-apply the next results",
                        self.palette.foreground,
                    ),
                    Line::styled("[n] Leave pending   [i] Details", self.palette.secondary),
                ];
                let control = Paragraph::new(Text::from(controls)).wrap(Wrap { trim: false });
                let height = control.line_count(area.width).min(area.height as usize) as u16;
                let upper = (Paragraph::new(Text::from(lines.clone()))
                    .wrap(Wrap { trim: false })
                    .line_count(area.width) as u16
                    + 1)
                .min(area.height.saturating_sub(height + 1));
                // If space is limited, keep goal/state/verification above controls.
                frame.render_widget(
                    Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
                    Rect::new(area.x, area.y, area.width, upper),
                );
                frame.render_widget(
                    control,
                    Rect::new(area.x, area.y + upper, area.width, height),
                );
                frame.render_widget(
                    Paragraph::new(if action.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "{} {action} {} Enter to choose",
                            if self.options.ascii { ">" } else { "›" },
                            if self.options.ascii { "-" } else { "·" }
                        )
                    })
                    .style(self.palette.focus),
                    Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
                );
            })?;
            let Some(event) = self.next().await? else {
                self.closed = true;
                return Ok(Input::Eof);
            };
            if let Event::Key(key) = event {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if key.code == KeyCode::Esc
                    || (key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c'))
                {
                    return Ok(Input::Cancel);
                }
                match key.code {
                    KeyCode::Enter => {
                        return Ok(Input::Submit(if action.is_empty() {
                            "d".into()
                        } else {
                            action
                        }));
                    }
                    KeyCode::Backspace => {
                        action.pop();
                    }
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.closed = true;
                        return Ok(Input::Eof);
                    }
                    // Never submits or edits the typed action; redraw shows the flip.
                    KeyCode::BackTab if self.auto_apply_mode => {
                        self.auto_apply = !self.auto_apply;
                    }
                    KeyCode::Char(c) if c.is_ascii_alphabetic() && action.len() < 12 => {
                        action.push(c.to_ascii_lowercase())
                    }
                    _ => {}
                }
            }
            // Pasted actions are deliberately not executable controls.
        }
    }

    pub(super) async fn inspect(
        &mut self,
        bundle: &mut ReviewBundle,
        view: &mut ReviewView,
        state: &State,
        target: &ReviewCommand,
        external: bool,
    ) -> Result<String> {
        self.input_boundary().await?;
        if self.options.plain {
            return self.inspect_plain(bundle, view).await;
        }
        let notice = if external && bundle.entries.len() <= 1 {
            self.external_reviewer(bundle, Some(view.selected), state, target, false)
                .await?
        } else {
            self.screen.take();
            self.screen = Some(Screen::open(true)?);
            if bundle.entries.len() == 1 {
                view.file = true;
            } else if external {
                view.file = false;
            }
            let result = self.inspect_loop(bundle, view, state, target).await;
            self.screen.take();
            self.screen = Some(Screen::new()?);
            self.render_key.clear();
            result?
        };
        self.input_boundary().await?;
        Ok(notice)
    }

    async fn inspect_loop(
        &mut self,
        bundle: &mut ReviewBundle,
        view: &mut ReviewView,
        state: &State,
        target: &ReviewCommand,
    ) -> Result<String> {
        let mut searching = false;
        let mut last_filter = None;
        let mut visible = Vec::new();
        let mut notice = String::new();
        loop {
            if last_filter.as_ref() != Some(&view.filter) {
                let filter = view.filter.to_lowercase();
                visible = bundle
                    .entries
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| {
                        entry
                            .path
                            .to_string_lossy()
                            .to_lowercase()
                            .contains(&filter)
                            || entry.previous_path.as_ref().is_some_and(|p| {
                                p.to_string_lossy().to_lowercase().contains(&filter)
                            })
                    })
                    .map(|(i, _)| i)
                    .collect::<Vec<_>>();
                last_filter = Some(view.filter.clone());
            }
            if !visible.contains(&view.selected)
                && let Some(first) = visible.first()
            {
                view.selected = *first;
            }
            let selected = bundle.entries.get(view.selected);
            let preview = if view.file && selected.is_some() {
                Some(bundle.preview(view.selected, view.offset, PAGE_BYTES)?)
            } else {
                None
            };
            let text = preview
                .as_ref()
                .map(|p| compact_patch(&p.text))
                .unwrap_or_default();
            let all_lines = diff_lines(&text, &self.palette);
            let ascii = self.options.ascii;
            let renames = bundle
                .entries
                .iter()
                .filter(|e| e.kind == crate::reviewer::ChangeKind::Renamed)
                .count();
            let screen = self.screen.as_mut().context("terminal is unavailable")?;
            screen.terminal.draw(|frame| {
                let area = content_area(frame.area());
                let body_height = area.height.saturating_sub(5) as usize;
                let title = if view.file {
                    selected
                        .map(|e| format!("Changes / {}", path_label(&e.path)))
                        .unwrap_or_else(|| "No changed files".into())
                } else {
                    format!(
                        "Changes  ·  {} entries  ·  {} matching",
                        bundle.entries.len(),
                        visible.len()
                    )
                };
                let title = if ascii {
                    title.replace('·', "-")
                } else {
                    title
                };
                frame.render_widget(
                    Paragraph::new(clip(&title, area.width)).style(self.palette.foreground.bold()),
                    Rect::new(area.x, area.y, area.width, 1),
                );
                if !view.file && renames > 0 {
                    frame.render_widget(
                        Paragraph::new(format!("{renames} rename(s) combine old and new paths"))
                            .style(self.palette.secondary),
                        Rect::new(area.x, area.y + 1, area.width, 1),
                    );
                }
                if view.file {
                    let lines: Vec<_> = all_lines
                        .iter()
                        .skip(view.row)
                        .take(body_height)
                        .cloned()
                        .collect::<Vec<_>>();
                    let clipped = lines
                        .iter()
                        .enumerate()
                        .filter_map(|(row, line)| {
                            (line.width() > usize::from(view.column) + usize::from(area.width))
                                .then_some(row)
                        })
                        .collect::<Vec<_>>();
                    frame.render_widget(
                        Paragraph::new(Text::from(lines)).scroll((0, view.column)),
                        Rect::new(area.x, area.y + 2, area.width, body_height as u16),
                    );
                    for row in clipped {
                        frame.render_widget(
                            Paragraph::new(if ascii { ">" } else { "›" })
                                .style(self.palette.warning),
                            Rect::new(
                                area.right().saturating_sub(1),
                                area.y + 2 + row as u16,
                                1,
                                1,
                            ),
                        );
                    }
                    if let Some(entry) = selected {
                        frame.render_widget(
                            Paragraph::new(
                                format!(
                                    "{} · {} · column {} · arrows pan",
                                    entry.kind.label(),
                                    entry.category(),
                                    view.column + 1
                                )
                                .replace('·', if ascii { "-" } else { "·" }),
                            )
                            .style(self.palette.secondary),
                            Rect::new(area.x, area.y + 1, area.width, 1),
                        );
                    }
                } else {
                    let at = visible
                        .iter()
                        .position(|i| *i == view.selected)
                        .unwrap_or(0);
                    let start = at.saturating_sub(body_height.saturating_sub(1));
                    let lines = visible
                        .iter()
                        .skip(start)
                        .take(body_height)
                        .map(|i| {
                            let e = &bundle.entries[*i];
                            let count = if e.category() != "text" {
                                e.category().into()
                            } else {
                                format!(
                                    "+{} -{}",
                                    e.additions.unwrap_or(0),
                                    e.deletions.unwrap_or(0)
                                )
                            };
                            let marker = if *i == view.selected { ">" } else { " " };
                            let path = match &e.previous_path {
                                Some(old) => {
                                    format!("{} <- {}", path_label(&e.path), path_label(old))
                                }
                                None => path_label(&e.path),
                            };
                            let row = if area.width < 60 {
                                let kind = format!(
                                    "{}{}",
                                    e.kind.label(),
                                    if e.permission_change {
                                        " mode"
                                    } else if e.binary {
                                        " binary"
                                    } else {
                                        ""
                                    }
                                );
                                format!(
                                    "{marker} {:8} {}",
                                    kind,
                                    tail_clip(
                                        &path,
                                        area.width.saturating_sub(kind.len().max(8) as u16 + 3)
                                    )
                                )
                            } else {
                                format!(
                                    "{marker} {:8} {:12} {}",
                                    e.kind.label(),
                                    count,
                                    tail_clip(&path, area.width.saturating_sub(25))
                                )
                            };
                            Line::styled(
                                clip(&row, area.width),
                                if *i == view.selected {
                                    self.palette.focus.bold()
                                } else {
                                    self.palette.foreground
                                },
                            )
                        })
                        .collect::<Vec<_>>();
                    frame.render_widget(
                        Paragraph::new(Text::from(lines)),
                        Rect::new(area.x, area.y + 2, area.width, body_height as u16),
                    );
                }
                let status = if searching {
                    format!("Filter /{}", view.filter)
                } else if !notice.is_empty() {
                    notice.clone()
                } else if preview.as_ref().is_some_and(|p| p.truncated) {
                    "Preview truncated · Space next page · v full patch".into()
                } else if view.file {
                    "Clipped lines: > · arrows pan · v full patch".into()
                } else {
                    format!("Filter /{}", view.filter)
                };
                let status = if ascii {
                    status.replace('·', "-")
                } else {
                    status
                };
                let hint = if area.width < 60 && view.file {
                    "q back · Esc files · arrows pan/scroll"
                } else if area.width < 60 {
                    "q back · Enter open · / find · e editor"
                } else if view.file {
                    "q back  Esc files  ↑↓ scroll  ←→ pan  n/p hunk  [ ] file  e editor  v pager"
                } else {
                    "q back  Enter open  / filter  ↑↓ choose  e editor  v pager"
                };
                frame.render_widget(
                    Paragraph::new(clip(&status, area.width)).style(self.palette.warning),
                    Rect::new(area.x, area.bottom().saturating_sub(2), area.width, 1),
                );
                frame.render_widget(
                    Paragraph::new(if ascii {
                        hint.replace("↑↓", "j/k")
                            .replace("←→", "h/l")
                            .replace('·', "-")
                    } else {
                        hint.into()
                    })
                    .style(self.palette.secondary),
                    Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
                );
            })?;
            let Some(event) = self.next().await? else {
                self.closed = true;
                return Ok(notice);
            };
            let Event::Key(key) = event else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
                self.closed = true;
                return Ok(notice);
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                return Ok(notice);
            }
            if searching {
                match key.code {
                    KeyCode::Esc | KeyCode::Enter => searching = false,
                    KeyCode::Backspace => {
                        view.filter.pop();
                    }
                    KeyCode::Char(c) if view.filter.len() < 1024 => view.filter.push(c),
                    _ => {}
                }
                continue;
            }
            let at = visible
                .iter()
                .position(|i| *i == view.selected)
                .unwrap_or(0);
            match key.code {
                KeyCode::Char('q') => return Ok(notice),
                KeyCode::Esc if view.file => {
                    view.file = false;
                    view.row = 0;
                }
                KeyCode::Esc => return Ok(notice),
                KeyCode::Char('/') if !view.file => searching = true,
                KeyCode::Enter if !view.file && !visible.is_empty() => {
                    view.file = true;
                    view.row = 0;
                    view.offset = 0;
                    view.column = 0;
                }
                KeyCode::Down | KeyCode::Char('j') if !view.file => {
                    if let Some(i) = visible.get((at + 1).min(visible.len().saturating_sub(1))) {
                        view.selected = *i;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') if !view.file => {
                    if let Some(i) = visible.get(at.saturating_sub(1)) {
                        view.selected = *i;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    view.row = (view.row + 1).min(all_lines.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => view.row = view.row.saturating_sub(1),
                KeyCode::PageDown => {
                    view.row = (view.row + 15).min(all_lines.len().saturating_sub(1))
                }
                KeyCode::PageUp => view.row = view.row.saturating_sub(15),
                KeyCode::Right | KeyCode::Char('l') => view.column = view.column.saturating_add(8),
                KeyCode::Left | KeyCode::Char('h') => view.column = view.column.saturating_sub(8),
                KeyCode::Home => {
                    view.offset = 0;
                    view.row = 0;
                    view.column = 0;
                }
                KeyCode::Char(' ') if view.file => {
                    if let Some(p) = &preview
                        && p.truncated
                    {
                        view.offset = p.next_offset;
                        view.row = 0;
                    }
                }
                KeyCode::Char('n' | 'p') if view.file => {
                    if let Some(e) = selected {
                        let next = if key.code == KeyCode::Char('n') {
                            e.hunks.iter().copied().find(|n| *n > view.offset)
                        } else {
                            e.hunks.iter().copied().rev().find(|n| *n < view.offset)
                        };
                        if let Some(next) = next {
                            view.offset = next;
                            view.row = 0;
                        }
                    }
                }
                KeyCode::Char(']' | '[') if view.file => {
                    let next = if key.code == KeyCode::Char(']') {
                        at + 1
                    } else {
                        at.saturating_sub(1)
                    };
                    if let Some(i) = visible.get(next) {
                        view.selected = *i;
                        view.offset = 0;
                        view.row = 0;
                        view.column = 0;
                    }
                }
                KeyCode::Char('e' | 'v') => {
                    notice = self
                        .external_reviewer(
                            bundle,
                            Some(view.selected),
                            state,
                            target,
                            key.code == KeyCode::Char('v'),
                        )
                        .await?;
                    self.input_boundary().await?;
                }
                _ => {}
            }
        }
    }

    async fn inspect_plain(
        &mut self,
        bundle: &ReviewBundle,
        view: &mut ReviewView,
    ) -> Result<String> {
        if bundle.entries.is_empty() {
            return Ok("No file changes to inspect.".into());
        }
        loop {
            if bundle.entries.len() == 1 {
                view.file = true;
            }
            let text = if view.file {
                let p = bundle.preview(view.selected, view.offset, 4096)?;
                let text = format!(
                    "{}\n{}\n{}",
                    path_label(&bundle.entries[view.selected].path),
                    compact_patch(&p.text),
                    if p.truncated {
                        "Bounded preview. [more] next page"
                    } else {
                        "End of file change"
                    }
                );
                view.offset = p.next_offset;
                text
            } else {
                bundle
                    .entries
                    .iter()
                    .enumerate()
                    .skip(view.selected)
                    .take(20)
                    .map(|(i, e)| format!("{} {} {}", i + 1, e.kind.label(), path_label(&e.path)))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            match self
                .command_prompt(&format!(
                    "{text}\n[number] open file  [more] next page  [files] file list  [q] back"
                ))
                .await?
            {
                Input::Submit(a) if a == "q" => return Ok(String::new()),
                Input::Submit(a) if a == "files" => {
                    view.file = false;
                    view.offset = 0;
                }
                Input::Submit(a) if a == "more" => {
                    if !view.file {
                        view.selected =
                            (view.selected + 20).min(bundle.entries.len().saturating_sub(1));
                    }
                }
                Input::Submit(a) => {
                    if let Ok(i) = a.parse::<usize>()
                        && i > 0
                        && i <= bundle.entries.len()
                    {
                        view.selected = i - 1;
                        view.file = true;
                        view.offset = 0;
                    }
                }
                _ => return Ok(String::new()),
            }
        }
    }

    pub(super) async fn diagnostics(&mut self, run: &RunRecord) -> Result<()> {
        if self.options.plain {
            self.commit(&details(run))?;
            let _ = self.command_prompt("[Enter] Back to review").await?;
            return Ok(());
        }
        self.input_boundary().await?;
        self.screen.take();
        self.screen = Some(Screen::open(true)?);
        let text = self.display_text(&details(run));
        let hint = if self.options.ascii {
            "Details - j/k scroll - q / Esc back"
        } else {
            "Details · j/k or ↑↓ scroll · q / Esc back"
        };
        let mut row = 0u16;
        let result: Result<()> = async {
            loop {
                self.screen.as_mut().unwrap().terminal.draw(|frame| {
                    let area = content_area(frame.area());
                    frame.render_widget(
                        Paragraph::new(text.as_str())
                            .wrap(Wrap { trim: false })
                            .scroll((row, 0)),
                        Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2)),
                    );
                    frame.render_widget(
                        Paragraph::new(hint).style(self.palette.secondary),
                        Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
                    );
                })?;
                match self.next().await? {
                    Some(Event::Key(k)) if matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) => {
                        break;
                    }
                    Some(Event::Key(k)) if matches!(k.code, KeyCode::Down | KeyCode::Char('j')) => {
                        row = row.saturating_add(1)
                    }
                    Some(Event::Key(k)) if matches!(k.code, KeyCode::Up | KeyCode::Char('k')) => {
                        row = row.saturating_sub(1)
                    }
                    Some(Event::Key(k))
                        if k.modifiers.contains(KeyModifiers::CONTROL)
                            && k.code == KeyCode::Char('c') =>
                    {
                        break;
                    }
                    None => {
                        self.closed = true;
                        break;
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        .await;
        self.screen.take();
        self.screen = Some(Screen::new()?);
        self.input_boundary().await?;
        result
    }

    async fn external_reviewer(
        &mut self,
        bundle: &mut ReviewBundle,
        index: Option<usize>,
        state: &State,
        target: &ReviewCommand,
        pager: bool,
    ) -> Result<String> {
        orchestrator::refresh_review_target(state, target)?;
        bundle.verify()?;
        let preference = if pager {
            Ok(ReviewerPreference::pager())
        } else {
            ReviewerPreference::discover(state)
        };
        let launch = preference.and_then(|p| {
            p.command(
                bundle,
                index,
                self.options.no_color || self.palette.accent.fg.is_none(),
                std::env::var("DISPATCH_THEME").is_ok_and(|s| s.eq_ignore_ascii_case("light")),
            )
        });
        let launch = match launch {
            Ok(l) => l,
            Err(e) => {
                return Ok(format!(
                    "Reviewer unavailable: {e}. Built-in review remains available."
                ));
            }
        };
        let result = super::handoff::run(self, launch).await;
        orchestrator::refresh_review_target(state, target)?;
        let integrity = bundle.verify()?;
        if integrity.copies_changed {
            return Ok("Review copies changed. Those edits are not part of the candidate and will not be applied.".into());
        }
        match result {
            Ok(status) if status.success() => {
                Ok("Review closed. Acceptance is still your choice.".into())
            }
            Ok(_) => Ok("Reviewer closed or failed. Built-in review remains available.".into()),
            Err(e) => Ok(format!(
                "Reviewer failed: {e}. Built-in review remains available."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_labels_preserve_unicode_and_escape_terminal_and_direction_controls() {
        assert_eq!(
            path_label(std::path::Path::new("src/λ\n\u{1b}[2J\u{202e}name.rs")),
            "src/λ\\n\\u{1b}[2J\\u{202e}name.rs"
        );
    }
}
