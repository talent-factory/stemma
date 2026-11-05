# plane-wave

workspace: tf
ci: just ci
hotspots: docs/chapters/06-*.tex, docs/chapters/12-cli-reference.tex, adr/
pr-language: English (commit and PR title in emoji conventional commit format)
extra-steps: `just book` when docs/ changes; update the chapter and the CLI reference with the code
forbidden: do not call `stemma` in a worktree; never run `gh workflow run` (ci.yml is started by Daniel); no `Co-Authored-By`; no `Claude-Session:` line in commits and PR texts
