use super::{TerminalView, frame::CellPosition};

impl TerminalView {
    pub(super) fn selection_range(&self) -> Option<(CellPosition, CellPosition)> {
        self.selection
            .map(|(a, b)| if a <= b { (a, b) } else { (b, a) })
    }

    pub(super) fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection_range()?;
        let mut copied = String::new();
        for row in start.row..=end.row {
            if row > start.row {
                copied.push('\n');
            }
            if let Some(line) = self.frame.rows.get(row) {
                let left = if row == start.row { start.column } else { 0 };
                let right = if row == end.row {
                    end.column.saturating_add(1)
                } else {
                    line.cells.len()
                };
                let text: String = line
                    .cells
                    .iter()
                    .skip(left)
                    .take(right.saturating_sub(left))
                    .map(|cell| cell.text.as_str())
                    .collect();
                copied.push_str(text.trim_end());
            }
        }
        Some(copied)
    }
}
