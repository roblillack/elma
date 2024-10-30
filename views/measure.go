package views

import (
	"github.com/gdamore/tcell"
	"github.com/mattn/go-runewidth"
	"github.com/rivo/tview"
	"github.com/rivo/uniseg"
)

// Taken from tview's util.go
func TextWidth(text string) (width int) {
	g := uniseg.NewGraphemes(text)
	for g.Next() {
		var chWidth int
		for _, r := range g.Runes() {
			chWidth = runewidth.RuneWidth(r)
			if chWidth > 0 {
				break // Our best guess at this point is to use the width of the first non-zero-width rune.
			}
		}
		width += chWidth
	}
	return
}

// iterateString iterates through the given string one printed character at a
// time. For each such character, the callback function is called with the
// Unicode code points of the character (the first rune and any combining runes
// which may be nil if there aren't any), the starting position (in bytes)
// within the original string, its length in bytes, the screen position of the
// character, and the screen width of it. The iteration stops if the callback
// returns true. This function returns true if the iteration was stopped before
// the last character.
func iterateString(text string, callback func(main rune, comb []rune, textPos, textWidth, screenPos, screenWidth int) bool) bool {
	var screenPos int

	gr := uniseg.NewGraphemes(text)
	for gr.Next() {
		r := gr.Runes()
		from, to := gr.Positions()
		width := TextWidth(gr.Str())
		var comb []rune
		if len(r) > 1 {
			comb = r[1:]
		}

		//		panic(fmt.Sprintf(`from=%d to=%d screenPos=%d width=%d`, from, to, screenPos, width))
		if callback(r[0], comb, from, to-from, screenPos, width) {
			return true
		}

		screenPos += width
	}

	return false
}

// overlayStyle mixes a background color with a foreground color (fgColor),
// a (possibly new) background color (bgColor), and style attributes, and
// returns the resulting style. For a definition of the colors and attributes,
// see styleFromTag(). Reset instructions cause the corresponding part of the
// default style to be used.
func overlayStyle(background tcell.Color, defaultStyle tcell.Style, fgColor, bgColor, attributes string) tcell.Style {
	defFg, defBg, defAttr := defaultStyle.Decompose()
	style := defaultStyle.Background(background)

	style = style.Foreground(defFg)
	if fgColor != "" {
		style = style.Foreground(tcell.GetColor(fgColor))
	}

	if bgColor == "-" || bgColor == "" && defBg != tcell.ColorDefault {
		style = style.Background(defBg)
	} else if bgColor != "" {
		style = style.Background(tcell.GetColor(bgColor))
	}

	if attributes == "-" {
		style = style.Bold(defAttr&tcell.AttrBold > 0)
		style = style.Blink(defAttr&tcell.AttrBlink > 0)
		style = style.Reverse(defAttr&tcell.AttrReverse > 0)
		style = style.Underline(defAttr&tcell.AttrUnderline > 0)
		style = style.Dim(defAttr&tcell.AttrDim > 0)
	} else if attributes != "" {
		style = style.Normal()
		for _, flag := range attributes {
			switch flag {
			case 'l':
				style = style.Blink(true)
			case 'b':
				style = style.Bold(true)
			case 'd':
				style = style.Dim(true)
			case 'r':
				style = style.Reverse(true)
			case 'u':
				style = style.Underline(true)
			}
		}
	}

	return style
}

// Print to the screen without interpreting color escape sequences
func Print(screen tcell.Screen, text string, x, y, maxWidth, align int, style tcell.Style) (int, int) {
	if maxWidth <= 0 || len(text) == 0 {
		return 0, 0
	}

	// Decompose the text.
	strippedWidth := TextWidth(text)

	// We want to reduce all alignments to AlignLeft.
	if align == tview.AlignRight {
		if strippedWidth <= maxWidth {
			// There's enough space for the entire text.
			return Print(screen, text, x+maxWidth-strippedWidth, y, maxWidth, tview.AlignLeft, style)
		}
	} else if align == tview.AlignCenter {
		if strippedWidth == maxWidth {
			// Use the exact space.
			return Print(screen, text, x, y, maxWidth, tview.AlignLeft, style)
		} else if strippedWidth < maxWidth {
			// We have more space than we need.
			half := (maxWidth - strippedWidth) / 2
			return Print(screen, text, x+half, y, maxWidth-half, tview.AlignLeft, style)
		} else {
			offset := strippedWidth - maxWidth
			return Print(screen, text[offset:], x, y, maxWidth, tview.AlignLeft, style)
		}
	}

	// Draw text.
	var (
		drawn, drawnWidth, tagOffset                 int
		foregroundColor, backgroundColor, attributes string
	)
	iterateString(text, func(main rune, comb []rune, textPos, length, screenPos, screenWidth int) bool {
		// Only continue if there is still space.
		if drawnWidth+screenWidth > maxWidth {
			return true
		}

		// Print the rune sequence.
		finalX := x + drawnWidth
		_, _, finalStyle, _ := screen.GetContent(finalX, y)
		_, background, _ := finalStyle.Decompose()
		finalStyle = overlayStyle(background, style, foregroundColor, backgroundColor, attributes)
		for offset := screenWidth - 1; offset >= 0; offset-- {
			// To avoid undesired effects, we populate all cells.
			if offset == 0 {
				screen.SetContent(finalX+offset, y, main, comb, finalStyle)
			} else {
				screen.SetContent(finalX+offset, y, ' ', nil, finalStyle)
			}
		}

		// Advance.
		drawn += length
		drawnWidth += screenWidth

		return false
	})

	return drawn + tagOffset, drawnWidth
}
