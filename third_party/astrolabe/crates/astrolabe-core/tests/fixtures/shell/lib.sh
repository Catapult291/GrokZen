#!/usr/bin/env bash
# Shell fixture: every declaration form the extractor must report.

APP_HOME=/opt/app
export EXPORTED=1 MORE=2
readonly VERSION="1.0"
declare -r LIMIT=10
declare -gr GLOBAL_RO=yes
declare -a ARR=(a b c)
typeset TYPESET_VAR=x
local NOT_A_SYMBOL=1

source ./common.sh
. helpers/util.bash
. "${BASH_SOURCE%/*}/dyn.sh"
source "$(dirname "$0")/sub.sh"

posix_fn() {
  local scoped=1
  loop_var=2
  echo "$scoped $loop_var"
}

function kw_fn {
  inner_fn() { :; }
  inner_fn
}

function kw_paren_fn() {
  posix_fn
}

_private_helper() { printf '%s\n' "$1"; }

if [ -n "$APP_HOME" ]; then
  in_if_fn() { :; }
  COND_VAR=1
fi

$DYNAMIC_CMD arg
