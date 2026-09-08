# Avada zsh integration — bundled ZDOTDIR, stage 2 of 2 (.zshrc).
#
# Runs for interactive shells only. Source the user's real .zshrc first (with their
# ZDOTDIR restored, so their prompt/aliases/plugins all apply), then chain in the
# avada cwd-reporting hook from hp-init.sh (sibling of this directory).
# Guarded throughout: any failure leaves a normal interactive zsh.
_avada_init="${${(%):-%x}:A:h:h}/hp-init.sh"
ZDOTDIR="${AVADA_ZDOTDIR_ORIG:-$HOME}"
unset AVADA_ZDOTDIR_ORIG
[ -f "$ZDOTDIR/.zshrc" ] && . "$ZDOTDIR/.zshrc"
[ -f "$_avada_init" ] && . "$_avada_init"
unset _avada_init
