package styles

import tcell "github.com/gdamore/tcell/v2"

// Ok, this is the version only working with attributes:
// var ActionBarStyle = tcell.StyleDefault.Reverse(true)
// var InfoBarStyle = tcell.StyleDefault.Reverse(true)

// And this is the version with _light_ color use, tested in dark and light modes:
var ActionBarStyle = tcell.StyleDefault.Background(tcell.ColorLightGray).Foreground(tcell.ColorDimGray)
var InfoBarStyle = tcell.StyleDefault.Background(tcell.ColorLightGray).Foreground(tcell.ColorDimGray)
var MessageListStyle = tcell.StyleDefault.Background(tcell.ColorDefault).Foreground(tcell.ColorDefault)

var MessageColorStarred = tcell.ColorDefault
var MessageAttributeStarred = tcell.AttrBold
var MessageColorNew = tcell.ColorRed
var MessageAttributeNew = tcell.AttrNone
var MessageColorArchived = tcell.ColorDarkCyan
var MessageAttributeArchived = tcell.AttrItalic
var MessageColorDeleted = tcell.ColorDefault
var MessageAttributeDeleted = tcell.AttrStrikeThrough | tcell.AttrDim
