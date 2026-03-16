/// Markdown → Slack mrkdwn 変換
///
/// Slack は独自の mrkdwn 記法を使うため、Claude が出力する標準 Markdown を変換する。
/// - `**bold**` → `*bold*`
/// - `### heading` → `*heading*`
/// - `~~strike~~` → `~strike~`
/// - `[text](url)` → `<url|text>`
/// - コードブロック・インラインコード・引用・リストはそのまま
pub fn markdown_to_mrkdwn(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut in_code_block = false;

    for line in text.lines() {
        if !result.is_empty() {
            result.push('\n');
        }

        // コードブロック内はそのまま出力
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            result.push_str(line);
            continue;
        }
        if in_code_block {
            result.push_str(line);
            continue;
        }

        // 見出し → 太字
        if let Some(heading) = strip_heading(line) {
            result.push_str(&format!("*{}*", heading));
            continue;
        }

        // インライン変換
        result.push_str(&convert_inline(line));
    }

    result
}

/// `## heading` / `### heading` などから見出しテキストを抽出
fn strip_heading(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        let content = trimmed.trim_start_matches('#').trim_start();
        if !content.is_empty() {
            return Some(content);
        }
    }
    None
}

/// 1行内のインライン Markdown を Slack mrkdwn に変換
fn convert_inline(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // インラインコード — そのまま通す
        if chars[i] == '`' {
            let start = i;
            i += 1;
            while i < len && chars[i] != '`' {
                i += 1;
            }
            if i < len {
                i += 1; // closing `
            }
            out.extend(&chars[start..i]);
            continue;
        }

        // Markdown リンク [text](url) → <url|text>
        if chars[i] == '[' {
            if let Some((text, url, end)) = parse_md_link(&chars, i) {
                out.push('<');
                out.push_str(&url);
                out.push('|');
                out.push_str(&text);
                out.push('>');
                i = end;
                continue;
            }
        }

        // **bold** → *bold*
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_closing(&chars, i + 2, &['*', '*']) {
                out.push('*');
                let inner: String = chars[i + 2..end].iter().collect();
                out.push_str(&convert_inline(&inner));
                out.push('*');
                i = end + 2;
                continue;
            }
        }

        // ~~strike~~ → ~strike~
        if i + 1 < len && chars[i] == '~' && chars[i + 1] == '~' {
            if let Some(end) = find_closing(&chars, i + 2, &['~', '~']) {
                out.push('~');
                let inner: String = chars[i + 2..end].iter().collect();
                out.push_str(&inner);
                out.push('~');
                i = end + 2;
                continue;
            }
        }

        out.push(chars[i]);
        i += 1;
    }

    out
}

/// `[text](url)` を解析。成功時は (text, url, end_index) を返す
fn parse_md_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    // start は '[' の位置
    let mut i = start + 1;
    let len = chars.len();

    // ] を探す
    let mut depth = 1;
    let text_start = i;
    while i < len && depth > 0 {
        match chars[i] {
            '[' => depth += 1,
            ']' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    if depth != 0 {
        return None;
    }
    let text: String = chars[text_start..i - 1].iter().collect();

    // 直後に ( が必要
    if i >= len || chars[i] != '(' {
        return None;
    }
    i += 1;
    let url_start = i;

    // ) を探す
    let mut paren_depth = 1;
    while i < len && paren_depth > 0 {
        match chars[i] {
            '(' => paren_depth += 1,
            ')' => paren_depth -= 1,
            _ => {}
        }
        i += 1;
    }
    if paren_depth != 0 {
        return None;
    }
    let url: String = chars[url_start..i - 1].iter().collect();

    Some((text, url, i))
}

/// 2文字の閉じシーケンスを探す
fn find_closing(chars: &[char], start: usize, pattern: &[char; 2]) -> Option<usize> {
    let len = chars.len();
    let mut i = start;
    while i + 1 < len {
        if chars[i] == '`' {
            // インラインコード内はスキップ
            i += 1;
            while i < len && chars[i] != '`' {
                i += 1;
            }
            if i < len {
                i += 1;
            }
            continue;
        }
        if chars[i] == pattern[0] && chars[i + 1] == pattern[1] {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bold() {
        assert_eq!(markdown_to_mrkdwn("**hello**"), "*hello*");
    }

    #[test]
    fn test_heading() {
        assert_eq!(markdown_to_mrkdwn("### Intent 分析結果"), "*Intent 分析結果*");
        assert_eq!(markdown_to_mrkdwn("## Summary"), "*Summary*");
    }

    #[test]
    fn test_strikethrough() {
        assert_eq!(markdown_to_mrkdwn("~~old~~"), "~old~");
    }

    #[test]
    fn test_link() {
        assert_eq!(
            markdown_to_mrkdwn("[click here](https://example.com)"),
            "<https://example.com|click here>"
        );
    }

    #[test]
    fn test_code_block_preserved() {
        let input = "before\n```\n**not bold**\n```\nafter **bold**";
        let expected = "before\n```\n**not bold**\n```\nafter *bold*";
        assert_eq!(markdown_to_mrkdwn(input), expected);
    }

    #[test]
    fn test_inline_code_preserved() {
        assert_eq!(markdown_to_mrkdwn("`**code**`"), "`**code**`");
    }

    #[test]
    fn test_mixed() {
        let input = "### 確認したい点\n\n- **明確さ**: incomplete\n- [参考](https://example.com)";
        let expected = "*確認したい点*\n\n- *明確さ*: incomplete\n- <https://example.com|参考>";
        assert_eq!(markdown_to_mrkdwn(input), expected);
    }

    #[test]
    fn test_nested_bold_in_link_text() {
        assert_eq!(
            markdown_to_mrkdwn("**[text](https://a.com)**"),
            "*<https://a.com|text>*"
        );
    }

    #[test]
    fn test_no_change() {
        assert_eq!(markdown_to_mrkdwn("plain text"), "plain text");
        assert_eq!(markdown_to_mrkdwn("> quote"), "> quote");
        assert_eq!(markdown_to_mrkdwn("- item"), "- item");
        assert_eq!(markdown_to_mrkdwn("*already slack bold*"), "*already slack bold*");
    }
}
