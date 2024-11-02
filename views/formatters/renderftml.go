package formatters

import (
	"fmt"
	"io"
	"math"
	"strings"

	"github.com/rivo/tview"
	"github.com/roblillack/ftml"
	"github.com/roblillack/ftml/formatter"
)

type Breadcrumbs []ftml.ParagraphType

type FTMLRenderer struct {
	Document *ftml.Document
	Writer   io.Writer

	currentLineSpacing int
}

func (f *FTMLRenderer) WriteString(s string) error {
	_, err := io.WriteString(f.Writer, tview.Escape(s))
	return err
}

func (f *FTMLRenderer) WriteRaw(s string) error {
	_, err := io.WriteString(f.Writer, s)
	return err
}

func WriteFTML(w io.Writer, d *ftml.Document) error {
	f := &FTMLRenderer{
		Writer:   w,
		Document: d,
	}

	return f.WriteParagraphs(d.Paragraphs, "", "")
}

const WrapWidth = 72

// As per tview's documentation:
// https://github.com/rivo/tview/blob/master/doc.go#L95
var styleTags = map[ftml.InlineStyle]struct{ B, E string }{
	ftml.StyleBold:      {"[::b]", "[::B]"},
	ftml.StyleItalic:    {"[::i]", "[::I]"},
	ftml.StyleHighlight: {"[::r]", "[::R]"},
	ftml.StyleUnderline: {"[::u]", "[::U]"},
	ftml.StyleCode:      {"", ""},
	ftml.StyleStrike:    {"[::s]", "[::S]"},
}

const resetAllModes = "[-:-:-:-]"

func (f *FTMLRenderer) WriteCentered(span ftml.Span, length int, followPrefix string) (int, error) {
	l := span.Width()

	for i := math.Floor(float64(WrapWidth-len(followPrefix))/2 - float64(l)/2); i > 0; i-- {
		if err := f.WriteRaw(" "); err != nil {
			return length, err
		}
		length++
	}

	return f.WriteSpan(span, length, followPrefix, formatter.StyleSet{})
}

func (f *FTMLRenderer) EmitLineBreak(followPrefix string, currentStyles formatter.StyleSet) (int, error) {
	modes := currentStyles.All()
	if len(modes) > 0 {
		if err := f.WriteRaw(resetAllModes); err != nil {
			return 0, err
		}
	}
	if err := f.WriteString("\n" + followPrefix); err != nil {
		return 0, err
	}
	if len(modes) > 0 {
		for _, i := range modes {
			if err := f.WriteRaw(styleTags[i].B); err != nil {
				return 0, err
			}
		}
	}
	return len([]rune(followPrefix)), nil
}

func (f *FTMLRenderer) WriteSpan(span ftml.Span, length int, followPrefix string, outerStyles formatter.StyleSet) (int, error) {
	currentStyles := outerStyles.Add(span.Style)
	tag := styleTags[span.Style]
	if tag.B != "" {
		if err := f.WriteRaw(tag.B); err != nil {
			return length, err
		}
	}

	if span.Style == ftml.StyleNone && span.Text == "\n" {
		return f.EmitLineBreak(followPrefix, currentStyles)
	} else if span.Style == ftml.StyleNone {
		for pos := 0; pos < len(span.Text); {
			for ws := pos; ws < len(span.Text); ws++ {
				if !strings.ContainsRune(" \t\n", rune(span.Text[ws])) {
					break
				}
				if err := f.WriteString(string(span.Text[ws])); err != nil {
					return length, err
				}
				length++
				pos++
			}
			if pos >= len(span.Text) {
				break
			}

			nextWs := strings.IndexAny(span.Text[pos:], " \t\n")
			if nextWs == -1 {
				if err := f.WriteString(span.Text[pos:]); err != nil {
					return length, err
				}
				length += len([]rune(span.Text[pos:]))
				break
			}

			word := span.Text[pos : pos+nextWs]
			wordLen := len([]rune(word))
			if length+wordLen > WrapWidth {
				if l, err := f.EmitLineBreak(followPrefix, currentStyles); err != nil {
					return 0, err
				} else {
					length = l
				}
			}

			if err := f.WriteString(word); err != nil {
				return length, err
			}
			length += wordLen
			pos += nextWs
		}
	} else {
		for _, child := range span.Children {
			l, err := f.WriteSpan(child, length, followPrefix, currentStyles)
			if err != nil {
				return length, err
			}
			length = l
		}
	}

	if tag.E != "" {
		if err := f.WriteRaw(tag.E); err != nil {
			return length, err
		}
	}

	return length, nil
}

func (f *FTMLRenderer) WriteParagraphs(paragraphs []*ftml.Paragraph, linePrefix, followPrefix string) error {
	for idx, c := range paragraphs {
		if idx > 0 {
			if err := f.WriteString(followPrefix + "\n"); err != nil {
				return err
			}
		}

		if err := f.WriteParagraph(c, linePrefix, followPrefix); err != nil {
			return err
		}

		linePrefix = followPrefix
	}

	return nil
}

func pad(s string, l int) string {
	for ; len(s) < l; s += " " {
	}
	return s
}

func (f *FTMLRenderer) WriteParagraph(p *ftml.Paragraph, linePrefix string, followPrefix string) error {
	if p.Leaf() {
		if err := f.WriteString(linePrefix); err != nil {
			return err
		}

		length := len([]rune(linePrefix))

		prev := 0
		next := 0
		boldHeaders := false
		underlineChar := ""
		prefix := ""

		switch p.Type {
		case ftml.Header1Paragraph:
			boldHeaders = true
			prev = 3
			next = 2
		case ftml.Header2Paragraph:
			underlineChar = "="
			boldHeaders = true
			prev = 2
			next = 1
		case ftml.Header3Paragraph:
			boldHeaders = true
			underlineChar = "-"
			prev = 1
		}

		if prev > f.currentLineSpacing {
			prev -= f.currentLineSpacing
		} else {
			prev = 0
		}
		f.currentLineSpacing = 0

		for i := 0; i < prev; i++ {
			if err := f.WriteString("\n" + followPrefix); err != nil {
				return err
			}
		}

		if err := f.WriteString(prefix); err != nil {
			return err
		}
		length += len([]rune(prefix))

		for _, c := range p.Content {
			if c.Text == "\n" {
				if err := f.WriteString(" \n" + followPrefix + prefix); err != nil {
					return err
				}
				length = len([]rune(followPrefix + prefix))
				continue
			}
			var l int
			var err error

			if boldHeaders {
				c = ftml.Span{
					Style:    ftml.StyleBold,
					Children: []ftml.Span{c},
				}
			}
			if p.Type == ftml.Header1Paragraph {
				l, err = f.WriteCentered(c, length, followPrefix)
			} else {
				l, err = f.WriteSpan(c, length, followPrefix, formatter.StyleSet{})
			}
			if err != nil {
				return err
			}
			length = l
		}

		if err := f.WriteRaw("\n"); err != nil {
			return err
		}

		if underlineChar != "" {
			l := len([]rune(prefix))
			for _, i := range p.Content {
				l += i.Width()
			}

			if err := f.WriteString(followPrefix); err != nil {
				return err
			}

			for i := 0; i < l; i++ {
				if err := f.WriteString(underlineChar); err != nil {
					return err
				}
			}

			if err := f.WriteRaw("\n"); err != nil {
				return err
			}
		}

		for i := 0; i < next; i++ {
			if err := f.WriteString(followPrefix + "\n"); err != nil {
				return err
			}
		}

		f.currentLineSpacing = next
		return nil
	}

	if p.Type == ftml.UnorderedListParagraph {
		for idx, entry := range p.Entries {
			if idx > 0 {
				if err := f.WriteString(followPrefix + "\n"); err != nil {
					return err
				}
			}
			if err := f.WriteParagraphs(entry, linePrefix+" • ", followPrefix+"   "); err != nil {
				return err
			}
			linePrefix = followPrefix
		}

		return nil
	}

	if p.Type == ftml.OrderedListParagraph {
		digits := len(fmt.Sprintf("%d", len(p.Entries)))

		for idx, entry := range p.Entries {
			if idx > 0 {
				if err := f.WriteString(followPrefix + "\n"); err != nil {
					return err
				}
			}
			if err := f.WriteParagraphs(entry, linePrefix+pad(fmt.Sprintf("%d. ", idx+1), digits+2), followPrefix+strings.Repeat(" ", digits+2)); err != nil {
				return err
			}
			linePrefix = followPrefix
		}

		return nil
	}

	if p.Type == ftml.QuoteParagraph {
		if err := f.WriteParagraphs(p.Children, linePrefix+"| ", followPrefix+"| "); err != nil {
			return err
		}

		return nil
	}

	return f.WriteParagraphs(p.Children, linePrefix, followPrefix)
}
