use anyhow::{Context, Result};
use tdoc::{
    Document,
    formatter::{Formatter, FormattingStyle},
};

pub fn render_document(document: &Document, width: u16) -> Result<Vec<String>> {
    let mut buffer = Vec::new();
    {
        let mut formatter = Formatter::new_ascii(&mut buffer);
        formatter.style = ascii_style(width);
        formatter
            .write_document(document)
            .context("failed to render FTML document")?;
    }

    let rendered =
        String::from_utf8(buffer).context("failed to decode rendered FTML output as UTF-8")?;
    let mut lines = rendered
        .lines()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    if rendered.ends_with('\n') {
        lines.push(String::new());
    }
    Ok(lines)
}

fn ascii_style(width: u16) -> FormattingStyle {
    let mut style = FormattingStyle::ascii();
    style.wrap_width = width.max(10) as usize;
    style.enable_osc8_hyperlinks = false;
    style
}
