#!/bin/bash
# Pong's web API from inside the container: set up an admin once, sign in, then CMD PATH [JSON].
W=https://localhost:47822; J=/tmp/pong-cookies
curl -sk -c $J -H 'content-type: application/json' -d '{"user":"admin","password":"test-password-1"}' $W/api/setup >/dev/null
curl -sk -c $J -H 'content-type: application/json' -d '{"user":"admin","password":"test-password-1"}' $W/api/login >/dev/null
case "$1" in
get) curl -sk -b $J "$W$2" ;;
post) curl -sk -b $J -H 'content-type: application/json' -d "${3:-{\}}" "$W$2" ;;
esac
