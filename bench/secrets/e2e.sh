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
  got=$("$FAB" --headless --browser "$BROWSER_NAME" "$@" 2>&1)
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
