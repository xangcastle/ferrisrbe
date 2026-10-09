//! Reduces Bazel's `Progress` stderr/stdout into the lines a terminal would
//! show at the end. Bazel redraws its progress bar with cursor-up and
//! erase-line sequences, so a naive concatenation repeats every redraw.

/// Terminal state rebuilt from a stream of output chunks. Bazel may split an
/// escape sequence across two `Progress` events, so the tail of an unfinished
/// sequence is kept until the next chunk arrives.
#[derive(Debug, Clone, Default)]
pub struct Console {
    /// Screen contents; the last line holds the cursor.
    lines: Vec<String>,
    /// Unterminated escape sequence carried over to the next chunk.
    pending: String,
    /// First line changed since the last `take_delta`.
    dirty_from: Option<usize>,
}

impl Console {
    /// Append a chunk of terminal output, applying the control sequences Bazel
    /// emits: `ESC[nA` (cursor up), `ESC[K` (erase line), `\r`. Every other
    /// CSI sequence (colors, cursor visibility) is dropped.
    pub fn feed(&mut self, chunk: &str) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        let input = if self.pending.is_empty() {
            chunk.to_string()
        } else {
            std::mem::take(&mut self.pending) + chunk
        };
        let mut chars = input.char_indices().peekable();
        while let Some((pos, c)) = chars.next() {
            match c {
                '\u{1b}' => {
                    let Some(&(_, next)) = chars.peek() else {
                        self.pending = input[pos..].to_string();
                        return;
                    };
                    if next != '[' {
                        continue;
                    }
                    chars.next();
                    let mut params = String::new();
                    let mut final_byte = None;
                    for (_, p) in chars.by_ref() {
                        if p.is_ascii_alphabetic() {
                            final_byte = Some(p);
                            break;
                        }
                        params.push(p);
                    }
                    let Some(final_byte) = final_byte else {
                        self.pending = input[pos..].to_string();
                        return;
                    };
                    self.apply_csi(&params, final_byte);
                }
                '\r' => self.clear_current(),
                '\n' => {
                    self.mark_dirty();
                    self.lines.push(String::new());
                }
                _ => {
                    self.mark_dirty();
                    if let Some(last) = self.lines.last_mut() {
                        last.push(c);
                    }
                }
            }
        }
    }

    /// Applies one complete CSI sequence. Cursor-up (`A`) never moves above the
    /// first line; erase (`K`, `J`) clears the cursor line; everything else is
    /// presentation-only and ignored.
    fn apply_csi(&mut self, params: &str, final_byte: char) {
        match final_byte {
            'A' => {
                let rows = params.parse::<usize>().unwrap_or(1).max(1);
                for _ in 0..rows {
                    if self.lines.len() > 1 {
                        self.lines.pop();
                    }
                }
                self.mark_dirty();
                self.clear_current();
            }
            'K' | 'J' => self.clear_current(),
            _ => {}
        }
    }

    fn clear_current(&mut self) {
        self.mark_dirty();
        if let Some(last) = self.lines.last_mut() {
            last.clear();
        }
    }

    fn mark_dirty(&mut self) {
        let current = self.lines.len().saturating_sub(1);
        self.dirty_from = Some(self.dirty_from.map_or(current, |d| d.min(current)));
    }

    /// Lines changed since the previous call: the index of the first one and
    /// everything from there to the end, so a client can splice its copy.
    pub fn take_delta(&mut self) -> Option<(usize, Vec<String>)> {
        let from = self.dirty_from.take()?;
        Some((from, self.lines[from..].to_vec()))
    }

    /// The screen as text, without trailing newlines.
    pub fn render(&self) -> String {
        let mut out = self.lines.join("\n");
        while out.ends_with('\n') {
            out.pop();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&str]) -> String {
        let mut console = Console::default();
        for c in chunks {
            console.feed(c);
        }
        console.render()
    }

    #[test]
    fn progress_redraws_collapse_into_the_final_line() {
        let out = run(&[
            "(00:01) Analyzing: 3 targets (1 packages loaded)\n",
            "\u{1b}[1A\u{1b}[K(00:02) Analyzing: 3 targets (10 packages loaded)\n",
            "\u{1b}[1A\u{1b}[K(00:03) \u{1b}[32mINFO: \u{1b}[0mAnalyzed 3 targets.\n",
            "(00:04) INFO: Build completed successfully\n",
        ]);
        assert_eq!(
            out,
            "(00:03) INFO: Analyzed 3 targets.\n(00:04) INFO: Build completed successfully"
        );
    }

    #[test]
    fn chunks_may_split_lines_and_sequences() {
        let out = run(&["INFO: Elapsed ", "time: 1.2s\n\u{1b}[", "1A\u{1b}[Kdone\n"]);
        assert_eq!(out, "done");
    }

    #[test]
    fn deltas_start_at_the_first_rewritten_line() {
        let mut console = Console::default();
        console.feed("a\nb\nc");
        assert_eq!(
            console.take_delta(),
            Some((0, vec!["a".into(), "b".into(), "c".into()]))
        );
        assert_eq!(console.take_delta(), None);
        console.feed("\u{1b}[1A\u{1b}[KB\nd");
        assert_eq!(
            console.take_delta(),
            Some((1, vec!["B".into(), "d".into()]))
        );
    }

    #[test]
    fn carriage_return_rewrites_the_current_line() {
        assert_eq!(run(&["Fetching 10%\rFetching 100%\n"]), "Fetching 100%");
    }
}
