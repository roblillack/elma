package formatters

import (
	"io"

	"github.com/roblillack/ftml"
	"github.com/roblillack/ftml/formatter"
)

const IndentWidth = 2

func WriteFTML(w io.Writer, d *ftml.Document, width int) error {
	st := formatter.DefaultFormattingStyle()
	st.LeftPadding = IndentWidth
	st.WrapWidth = width - IndentWidth

	// As per tview's documentation:
	// https://github.com/rivo/tview/blob/master/doc.go#L95
	st.ResetStyles = "[-:-:-:-]"
	st.TextStyles = map[ftml.InlineStyle]formatter.StyleTags{
		ftml.StyleBold:      {"[::b]", "[::B]"},
		ftml.StyleItalic:    {"[::i]", "[::I]"},
		ftml.StyleHighlight: {"[::r]", "[::R]"},
		ftml.StyleUnderline: {"[::u]", "[::U]"},
		ftml.StyleCode:      {"", ""},
		ftml.StyleStrike:    {"[::s]", "[::S]"},
	}
	return formatter.New(w, st).WriteDocument(d)
}
