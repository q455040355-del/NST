//! 把 OCR 的行框子还原成表格。
//!
//! PaddleOCR 只给「一行文字 + 它的外框」，没有表格线。这里纯按几何还原：
//! 纵向中心挨得近的行框并成同一行，**任何一行都没压到的横向空白**切成列 ——
//! 所以合并单元格、歪的表格会切不准，宁可返回 `None` 让调用方退回普通文字。
//!
//! 这一层不碰剪贴板、不碰 UI：`tsv()` / `html()` 只是生成字符串，单测直接喂矩形。

/// 一行 OCR 结果：文字 + 外框（图片坐标，左上右下）。
#[derive(Clone, Debug, PartialEq)]
pub struct LineBox {
    pub text: String,
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// 还原出来的网格（每行等宽，缺的格子是空串）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    cells: Vec<Vec<String>>,
}

impl Table {
    pub fn rows(&self) -> usize {
        self.cells.len()
    }

    /// 每行等宽，所以取第一行就行（构造时保证非空）
    pub fn cols(&self) -> usize {
        self.cells.first().map_or(0, Vec::len)
    }

    pub fn cell(&self, row: usize, col: usize) -> &str {
        self.cells
            .get(row)
            .and_then(|cells| cells.get(col))
            .map_or("", String::as_str)
    }

    /// 制表符分隔：Excel / WPS / Word 表格里一贴就按格分配（CRLF，和 EDIT 一致）
    pub fn tsv(&self) -> String {
        let mut out = String::with_capacity(self.cells.len() * self.cols() * 8);
        for (index, row) in self.cells.iter().enumerate() {
            if index > 0 {
                out.push_str("\r\n");
            }
            for (col, cell) in row.iter().enumerate() {
                if col > 0 {
                    out.push('\t');
                }
                out.push_str(&sanitize(cell));
            }
        }
        out
    }

    /// 最小 HTML 表格（不带上色/边框样式，交给目标程序自己的默认表格样式）。
    /// 粘到 Word / 网页里是真正的表格；粘到 Excel 里照样是格子。
    pub fn html(&self) -> String {
        let mut out = String::with_capacity(self.cells.len() * self.cols() * 16);
        out.push_str("<table>");
        for row in &self.cells {
            out.push_str("<tr>");
            for cell in row {
                out.push_str("<td>");
                escape_into(&mut out, &sanitize(cell));
                out.push_str("</td>");
            }
            out.push_str("</tr>");
        }
        out.push_str("</table>");
        out
    }
}

/// 从一堆行框子里还原表格；不像表格就返回 `None`。
pub fn detect(lines: &[LineBox]) -> Option<Table> {
    let mut boxes: Vec<&LineBox> = lines
        .iter()
        .filter(|line| {
            !line.text.trim().is_empty()
                && line.right > line.left
                && line.bottom > line.top
                && line.left.is_finite()
                && line.top.is_finite()
                && line.right.is_finite()
                && line.bottom.is_finite()
        })
        .collect();

    // 一行文字都凑不够，谈不上表格
    if boxes.len() < 4 {
        return None;
    }

    let mut heights: Vec<f32> = boxes.iter().map(|line| line.bottom - line.top).collect();
    heights.sort_by(f32::total_cmp);
    let median_height = heights[heights.len() / 2].max(1.0);

    // 1. 行：按竖直中心聚类。同一行的框子中心几乎相同，行距再紧也不会串行。
    boxes.sort_by(|a, b| {
        center_y(a)
            .total_cmp(&center_y(b))
            .then_with(|| a.left.total_cmp(&b.left))
    });
    let mut rows: Vec<Vec<&LineBox>> = Vec::new();
    for item in boxes {
        let fits = rows.last().is_some_and(|row| {
            let mean = row.iter().map(|line| center_y(line)).sum::<f32>() / row.len() as f32;
            (center_y(item) - mean).abs() <= median_height * 0.5
        });
        if fits {
            rows.last_mut().expect("刚判断过非空").push(item);
        } else {
            rows.push(vec![item]);
        }
    }
    if rows.len() < 2 {
        return None;
    }

    // 2. 列：横向投影。某个 x 上没有任何一行有字，就是列之间的空白。
    let left_edge = boxes_left_edge(&rows);
    let right_edge = boxes_right_edge(&rows);
    let span = right_edge - left_edge;
    if !span.is_finite() || span <= 1.0 {
        return None;
    }
    let width = span.ceil() as usize;
    if width > 20_000 {
        return None;
    }

    let mut covered = vec![false; width];
    for row in &rows {
        for line in row {
            let start = ((line.left - left_edge).floor().max(0.0) as usize).min(width - 1);
            let end = ((line.right - left_edge).ceil().max(0.0) as usize).min(width);
            for slot in &mut covered[start..end.max(start + 1)] {
                *slot = true;
            }
        }
    }

    // 太窄的缝不算列间距（文字自己贴得近，或被抗锯齿糊住）
    let min_gap = (median_height * 0.3).max(2.0).ceil() as usize;
    let mut cuts = Vec::with_capacity(8);
    let mut cursor = 0usize;
    while cursor < width {
        if covered[cursor] {
            cursor += 1;
            continue;
        }
        let start = cursor;
        while cursor < width && !covered[cursor] {
            cursor += 1;
        }
        let end = cursor;
        // 表头/表尾外的空白不是分隔线
        if start == 0 || end == width {
            continue;
        }
        if end - start >= min_gap {
            cuts.push(left_edge + (start + end) as f32 * 0.5);
        }
    }

    let mut edges = Vec::with_capacity(cuts.len() + 2);
    edges.push(left_edge);
    edges.extend(cuts);
    edges.push(right_edge);
    let column_count = edges.len() - 1;
    if column_count < 2 {
        return None;
    }

    // 3. 落格：外框之间没有交叉（交叠的空白早被覆盖掉了），取重叠最多的那一列
    let mut cells = vec![vec![String::new(); column_count]; rows.len()];
    let mut used = vec![false; column_count];
    for (row_index, row) in rows.iter().enumerate() {
        for line in row {
            let mut best = 0usize;
            let mut best_overlap = 0.0f32;
            for column in 0..column_count {
                let overlap = (edges[column + 1].min(line.right) - edges[column].max(line.left)).max(0.0);
                if overlap > best_overlap {
                    best_overlap = overlap;
                    best = column;
                }
            }
            if best_overlap <= 0.0 {
                continue;
            }
            used[best] = true;
            let slot = &mut cells[row_index][best];
            if !slot.is_empty() {
                slot.push(' ');
            }
            slot.push_str(line.text.trim());
        }
    }

    // 4. 校验：空列直接丢掉；每行至少得有一格，且多数行要有多格 ——
    //    单列文字、两栏正文（每行只有一格的）都不算表格。
    let kept: Vec<usize> = (0..column_count).filter(|&column| used[column]).collect();
    if kept.len() < 2 {
        return None;
    }
    let mut table = Vec::with_capacity(cells.len());
    for row in cells {
        let row: Vec<String> = kept.iter().map(|&column| row[column].clone()).collect();
        table.push(row);
    }

    let filled = table
        .iter()
        .filter(|row| row.iter().filter(|cell| !cell.is_empty()).count() >= 2)
        .count();
    if filled < 2 || filled * 5 < table.len() * 3 {
        return None;
    }

    Some(Table { cells: table })
}

fn center_y(line: &LineBox) -> f32 {
    (line.top + line.bottom) * 0.5
}

fn boxes_left_edge(rows: &[Vec<&LineBox>]) -> f32 {
    rows.iter()
        .flatten()
        .map(|line| line.left)
        .fold(f32::INFINITY, f32::min)
}

fn boxes_right_edge(rows: &[Vec<&LineBox>]) -> f32 {
    rows.iter()
        .flatten()
        .map(|line| line.right)
        .fold(f32::NEG_INFINITY, f32::max)
}

/// 格子里的换行/制表符会把 TSV 结构弄坏，统一压成空格
fn sanitize(cell: &str) -> String {
    let mut out = String::with_capacity(cell.len());
    let mut space = false;
    for ch in cell.chars() {
        if ch.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(ch);
    }
    out
}

fn escape_into(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个格子：左、右边界决定列，顶、底决定行
    fn cell(text: &str, left: f32, top: f32, right: f32, bottom: f32) -> LineBox {
        LineBox {
            text: text.to_string(),
            left,
            top,
            right,
            bottom,
        }
    }

    /// 一个 3 列 × 2 行的规整表格
    fn grid() -> Vec<LineBox> {
        vec![
            cell("姓名", 10.0, 10.0, 60.0, 26.0),
            cell("年龄", 100.0, 10.0, 150.0, 26.0),
            cell("城市", 200.0, 10.0, 250.0, 26.0),
            cell("张三", 10.0, 40.0, 60.0, 56.0),
            cell("28", 100.0, 40.0, 150.0, 56.0),
            cell("杭州", 200.0, 40.0, 250.0, 56.0),
        ]
    }

    #[test]
    fn rebuilt_grid_keeps_rows_and_columns() {
        let table = detect(&grid()).expect("规整的三列表格应该认出来");
        assert_eq!(table.cols(), 3);
        assert_eq!(table.rows(), 2);
        assert_eq!(table.cell(0, 0), "姓名");
        assert_eq!(table.cell(1, 2), "杭州");
        assert_eq!(table.tsv(), "姓名\t年龄\t城市\r\n张三\t28\t杭州");
    }

    #[test]
    fn missing_cell_becomes_an_empty_one() {
        let mut lines = grid();
        lines.pop(); // 最后一格丢了
        let table = detect(&lines).expect("缺一格不影响整张表");
        assert_eq!(table.rows(), 2);
        assert_eq!(table.cols(), 3);
        assert_eq!(table.cell(1, 2), "");
        assert_eq!(table.tsv(), "姓名\t年龄\t城市\r\n张三\t28\t");
    }

    #[test]
    fn single_column_text_is_not_a_table() {
        let lines = vec![
            cell("第一行", 10.0, 10.0, 200.0, 26.0),
            cell("第二行", 10.0, 34.0, 200.0, 50.0),
            cell("第三行", 10.0, 58.0, 200.0, 74.0),
        ];
        assert!(detect(&lines).is_none());
    }

    #[test]
    fn one_cell_per_row_is_not_a_table() {
        // 左栏一句、右栏一句，但两块文字的行对不齐 —— 这是分栏正文，不是表格
        let lines = vec![
            cell("左边第一句", 10.0, 10.0, 160.0, 26.0),
            cell("右边第一句", 200.0, 20.0, 350.0, 36.0),
            cell("左边第二句", 10.0, 40.0, 160.0, 56.0),
            cell("右边第二句", 200.0, 50.0, 350.0, 66.0),
        ];
        assert!(detect(&lines).is_none(), "每行只有一格，不能当表格");
    }

    #[test]
    fn two_columns_that_line_up_look_like_a_table() {
        // 两栏正文如果每一行都精确对齐，几何上和两列表格没有区别。
        // 这里只钉住「认出来」；要不要自动按表格复制由界面决定（只对 3 列以上自动）。
        let lines = vec![
            cell("左边第一句", 10.0, 10.0, 160.0, 26.0),
            cell("右边第一句", 200.0, 10.0, 350.0, 26.0),
            cell("左边第二句", 10.0, 34.0, 160.0, 50.0),
            cell("右边第二句", 200.0, 34.0, 350.0, 50.0),
        ];
        let table = detect(&lines).expect("几何上就是两列两行");
        assert_eq!((table.cols(), table.rows()), (2, 2));
    }

    #[test]
    fn text_indented_like_a_table_needs_shared_gaps() {
        // 第 2 行那一格横跨了整个缝，说明那只是普通文字
        let lines = vec![
            cell("项目", 10.0, 10.0, 60.0, 26.0),
            cell("数量", 120.0, 10.0, 170.0, 26.0),
            cell("很长的一整句话把中间的空白全压住了", 10.0, 40.0, 300.0, 56.0),
            cell("短", 10.0, 70.0, 60.0, 86.0),
            cell("12", 120.0, 70.0, 170.0, 86.0),
        ];
        assert!(detect(&lines).is_none(), "有格子横跨列缝时不能硬切");
    }

    #[test]
    fn html_escapes_markup() {
        let lines = vec![
            cell("a<b", 10.0, 10.0, 60.0, 26.0),
            cell("c&d", 100.0, 10.0, 150.0, 26.0),
            cell("1", 10.0, 40.0, 60.0, 56.0),
            cell("2", 100.0, 40.0, 150.0, 56.0),
        ];
        let table = detect(&lines).expect("两列两行也是表格");
        let html = table.html();
        assert!(html.contains("<td>a&lt;b</td>"));
        assert!(html.contains("<td>c&amp;d</td>"));
    }

    #[test]
    fn rows_with_different_heights_still_line_up() {
        let lines = vec![
            cell("标题一", 10.0, 10.0, 80.0, 34.0),
            cell("标题二", 120.0, 12.0, 180.0, 32.0),
            cell("小字", 10.0, 50.0, 60.0, 62.0),
            cell("小字", 120.0, 50.0, 170.0, 62.0),
        ];
        let table = detect(&lines).expect("行高不一样也能对上");
        assert_eq!((table.cols(), table.rows()), (2, 2));
    }

    #[test]
    fn tabs_inside_cells_are_flattened() {
        let lines = vec![
            cell("a\tb", 10.0, 10.0, 60.0, 26.0),
            cell("c", 100.0, 10.0, 150.0, 26.0),
            cell("d", 10.0, 40.0, 60.0, 56.0),
            cell("e", 100.0, 40.0, 150.0, 56.0),
        ];
        let table = detect(&lines).expect("还是表格");
        assert_eq!(table.tsv(), "a b\tc\r\nd\te");
    }
}
