# Ygg themes

Theme customization is disabled in Ygg v0.6.2. The terminal and graphical Serve
frontend always use Ygg's compiled default theme.

The `/theme` command is not registered, and the runtime does not discover or
load bundled or filesystem theme files. Legacy `--theme`, `--theme-dir`,
`theme` configuration, and `YGG_THEME` inputs are retained only for launch
compatibility: they are ignored or fall back to the compiled default.

The typed theme implementation and schema remain in the source tree so theme
support can be restored in a later release. They are not part of the v0.6.2
user-facing customization contract.
