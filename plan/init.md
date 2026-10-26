# about
* vortex is a modal text editor focusing on coding tasks with a sleek ux and minimal, sane defaults.
* editor uses vim motions and grammar for text manipulation

# tech
* async
* ratatui
* toml config file (kebab-case)
* text rope (or other recommendation for performance)

# tui
follow vim paradigm for modes: insert, normal, visual

## two main areas:
* text editing:
    * area of TUI with one or more panes containing text
    * panes may contain the _same_ text buffer or _different_
    * panes can be split and re-arranged via commands in "command mode"
* status area:
    * configurable height 
    * configurable position: top or bottom of TUI
    * displays:
        * current mode: "insert" or "command"
        * name of focused text editing pane
        * lsp rollup information (defer until later)
        * this area can be expanded in the future with more features

## ux notes:
* all panes should have a border + label to indicate focus and visually separate elements.
* colorscheme should follow terminal theme.
* ensure cursor is visible in 'normal' and 'visual' modes

# key features
* tree-sitter integration
* syntax highlighting
* spell checking integration
* automatic formatting for code input
    * detect language in file -> apply appropriate formatting as user enters text
* splitting text area can function in three ways:
    * explicit: "split this pane vertically/horizontally" -> bisects current pane in half
    * auto: select the dimension with largest amount of space and split accordingly
    * golden: resize all splits when focus changes according to the "golden ratio" (approximate the ratio if necessary)
* lsp integration (defer until later, ensure scaffold is prepared for this)
* integrated ripgrep for file and text searching
    * search for files to open
    * search project for text in files
