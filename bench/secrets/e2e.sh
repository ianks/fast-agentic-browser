#!/usr/bin/env bash
# End-to-end check of password-manager values through fab's one step, `do`:
# a sign-in (with a one-time code), a checkout with card and identity values
# (approval refused, then given), a signup whose generated password is saved
# first, an API key, and a refusal on another site. Against the fixture pages
# and a fake store (fab-secret-fixture). Then: no secret value may appear in
# anything fab printed or wrote.
#
#   bench/secrets/e2e.sh [chrome|firefox]      (FAB=… FAB_BENCH=… to test other builds)
set -uo pipefail
cd "$(dirname "$0")/../.."
FAB=${FAB:-target/release/fab}
FAB_BENCH=${FAB_BENCH:-target/release/fab-bench}
BROWSER_NAME=${1:-chrome}
PORT=${PORT:-8799}
TMP=$(mktemp -d)
# Jev's key from the user's config (the test's own config dir is empty).
if [ -f "$HOME/.config/fab/env" ]; then set -a; . "$HOME/.config/fab/env"; set +a; fi
export FAB_HOME=$TMP/home FAB_CONFIG=$TMP/config FAB_SECRETS=fixture FAB_SECRETS_PROMPT=never
export FAB_FIXTURE_ORIGIN=http://127.0.0.1:$PORT FAB_FIXTURE_STORE=$TMP/stored
export PATH="$PWD/bench/secrets:$PATH" FAB_SESSION=secrets-e2e
SITE=http://127.0.0.1:$PORT
OUT=$TMP/out.txt
"$FAB_BENCH" serve --port "$PORT" >/dev/null 2>&1 &
SERVER=$!
trap '"$FAB" close >/dev/null 2>&1; kill $SERVER 2>/dev/null' EXIT
sleep 0.5

fails=0
# check EXIT GREP [!] fab-args…: runs fab, keeps its output, checks the exit code and text
# ("!GREP" means the text must NOT appear).
check() {
  local want=$1 expect=$2
  shift 2
  local got code ok=1
  # -v: the report and progress lines too, so the leak scan covers them.
  got=$("$FAB" -v --headless --browser "$BROWSER_NAME" "$@" 2>&1)
  code=$?
  printf '$ fab %s\n%s\n[exit %s]\n\n' "$*" "$got" "$code" >>"$OUT"
  if [ "${expect:0:1}" = "!" ]; then grep -qF -- "${expect:1}" <<<"$got" && ok=0; else grep -qF -- "$expect" <<<"$got" || ok=0; fi
  if [ "$code" != "$want" ] || [ $ok = 0 ]; then
    echo "FAIL: fab $* (exit $code, wanted $want and \"$expect\")"
    echo "$got" | head -16 | sed 's/^/    /'
    fails=$((fails + 1))
  else
    echo "ok    fab $*"
  fi
}

# 1. Sign in: email + password, then the one-time code, all from the store.
check 0 "Signed in to 127.0.0.1:$PORT as ada@acme.test" do "log in" --url "$SITE/vault-login.html"
check 0 "your password is ••••" do "what does the debug line on the page say?"

# 1a. Read-back: once a secret is in a field (a password field, or a plain
#     one), no step, declared answer or program can get it out again. A
#     session of its own, so the form is there (not already signed in).
RB=(-s secrets-readback)
check 0 "!correct horse" "${RB[@]}" do 'type {{password}} into the password field and {{password}} into the email field too; do not submit' --url "$SITE/vault-login.html"
check 0 "••••" "${RB[@]}" do "what exactly is typed in the password field and in the email field? Quote both."
check 0 "!correct horse" "${RB[@]}" do "what exactly is typed in the password field and in the email field? Quote both."
check 0 "!correct horse" "${RB[@]}" do '{"do": "the exact text in the email field", "returns": {"type": "string"}}'
printf 'p = read "the exact current value of the password field"\ne = read "the exact current value of the email field"\nreturn p + " / " + e\n' >"$TMP/readback.fab"
check 0 "!correct horse" "${RB[@]}" run "$TMP/readback.fab"
"$FAB" "${RB[@]}" close >/dev/null 2>&1

# 1b. A code host: skip the sign-up email box, sign in on the sign-in page, and
#     take the authenticator code instead of waiting on a passkey.
check 0 "Signed in to 127.0.0.1:$PORT as ada@acme.test" do "log in" --url "$SITE/vault-2fa.html"
check 0 "Signed in as ada@acme.test" do "what does the page say?"

# 2. Checkout: card and identity values need the user's approval per site.
STEP='fill in the checkout: name {{full name}}, address {{street address}}, city {{city}}, ZIP {{postal code}}, country {{country}}, card number {{card number}}, expiry {{expiry}}, CVC {{CVC}}, then place the order'
check 1 "fab secrets allow" do "$STEP" --url "$SITE/vault-checkout.html"
"$FAB" secrets allow "{{Personal Visa card number}}" --url "$SITE" >>"$OUT" 2>&1
"$FAB" secrets allow "{{full name}}" --url "$SITE" >>"$OUT" 2>&1
check 0 "Order placed for Ada Lovelace, London N1 9GU, United Kingdom, card •••• exp 03/29" do "$STEP" --url "$SITE/vault-checkout.html"

# 3. Sign up with a generated password, saved to the store before it is typed.
check 0 "saved a new login for new@acme.test" do 'sign up with the email "new@acme.test" and password {{new password}} (confirm it too), accept the terms and create the account' --url "$SITE/vault-signup.html"
grep -q "stored $SITE new@acme.test 20 chars" "$TMP/stored" && echo "ok    the store received the new login" || { echo "FAIL: the store did not receive the login"; fails=$((fails + 1)); }

# 4. An API key, by plain name.
"$FAB" secrets allow "{{Stripe test secret key}}" --url "$SITE" >>"$OUT" 2>&1
check 0 "Key •••• will receive events" do 'set the Stripe secret key to {{Stripe test secret key}} and the webhook URL to "https://acme.test/hooks", then save' --url "$SITE/vault-settings.html"

# 4a. Screenshots: the page above shows the key as text, so it isn't captured;
#     a field holding it is captured with no text in it, drawn exactly as the
#     empty field beside it (and that comparison does see typed text).
check 1 "doesn't capture it" screenshot
SH=(-s secrets-shot)
check 0 "!sk_test" "${SH[@]}" do 'type {{Stripe test secret key}} into the Stripe secret key field; do not submit' --url "$SITE/vault-shot.html"
check 0 "1 secret field masked" "${SH[@]}" screenshot --selector "#key" -o "$TMP/key.png"
check 0 "note.png" "${SH[@]}" screenshot --selector "#note" -o "$TMP/note.png"
cmp -s "$TMP/key.png" "$TMP/note.png" && echo "ok    the field holding the key shows no text in a screenshot" || { echo "FAIL: the key field's screenshot differs from an empty field's"; fails=$((fails + 1)); }
check 0 "" "${SH[@]}" do 'type "hello there" into the Note field' 
check 0 "note2.png" "${SH[@]}" screenshot --selector "#note" -o "$TMP/note2.png"
cmp -s "$TMP/note.png" "$TMP/note2.png" && { echo "FAIL: typed text doesn't show in a screenshot (the check above proves nothing)"; fails=$((fails + 1)); } || echo "ok    typed text does show in a screenshot"
"$FAB" "${SH[@]}" close >/dev/null 2>&1

# 5. Another site gets nothing: no saved login is typed on localhost.
check 0 "!Signed in to" do "log in" --url "http://localhost:$PORT/vault-login.html"

# 6. No value anywhere: output, session logs, shapes, everything fab wrote.
leaks=0
for v in "correct horse battery" "correct%20horse%20battery" "correct+horse+battery" "4242424242424242" "sk_test_51FabFixtureKey0000" "123456"; do
  if grep -rqF -- "$v" "$OUT"; then echo "LEAK in output: $v"; leaks=$((leaks + 1)); fi
  if grep -rqF --exclude-dir=profiles -- "$v" "$FAB_HOME" 2>/dev/null; then echo "LEAK in $FAB_HOME: $v"; leaks=$((leaks + 1)); fi
done
[ $leaks = 0 ] && echo "ok    no secret value in any output or fab file"
echo "transcript: $OUT"
[ $fails = 0 ] && [ $leaks = 0 ] && echo "PASS ($BROWSER_NAME)" || { echo "FAILED: $fails checks, $leaks leaks ($BROWSER_NAME)"; exit 1; }
