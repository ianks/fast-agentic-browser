#!/bin/bash
# Planner-selection pilot (P0). Canonical goals; rescore strictly once allow-lists exist:
#   for f in bench/results/pilot-*.json; do fab rescore $f --suite <suite>; done
cd "$(dirname "$0")/.."
B=./target/release/fab
CHEAP=google/gemini-3.1-flash-lite,openai/gpt-6-luna,inception/mercury-2.5,stepfun/step-3.7-flash,z-ai/glm-5.3-flash,google/gemini-3.8-flash,deepseek/deepseek-v4.1-flash
STRONG=openai/gpt-6-sol,anthropic/claude-sonnet-5.5,anthropic/claude-opus-5.5
C6=login_noquote,checkout,confirm_delete,invoice_row,spa_tabs,datepicker
H6=parcelwise_hold_overdue,orders_refund_total,laptop_most_ram_under_budget,session_expiry_resave,expense_report_submit,audit_log_revoked_key
C3=checkout,confirm_delete,spa_tabs
H3=parcelwise_hold_overdue,orders_refund_total,session_expiry_resave
run() { $B bench --lenient --suite "$1" --mode "$2" --only "$3" --planner-model "$4" --label "$5" > bench/results/$5.log 2>&1; echo "done $5: $(grep -c PASS bench/results/$5.log) pass / $(grep -cE 'PASS|FAIL' bench/results/$5.log)"; }
run bench/scenarios.toml experiment $C6 $CHEAP pilot-comp-exp-cheap &
run bench/scenarios.toml control    $C6 $CHEAP pilot-comp-ctl-cheap &
run bench/hard.toml      experiment $H6 $CHEAP pilot-hard-exp-cheap &
run bench/hard.toml      control    $H6 $CHEAP pilot-hard-ctl-cheap &
run bench/scenarios.toml experiment $C3 $STRONG pilot-comp-exp-strong &
run bench/scenarios.toml control    $C3 $STRONG pilot-comp-ctl-strong &
run bench/hard.toml      experiment $H3 $STRONG pilot-hard-exp-strong &
run bench/hard.toml      control    $H3 $STRONG pilot-hard-ctl-strong &
wait
