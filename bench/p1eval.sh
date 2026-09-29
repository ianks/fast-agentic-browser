#!/bin/bash
# P1 evaluation: experiment arm with the P1 binary on the pilot tasks/models (paired with pilot-*-exp-*).
cd "$(dirname "$0")/.."
B=./target/release/fab
CHEAP=google/gemini-3.1-flash-lite,openai/gpt-6-luna,inception/mercury-2.5,stepfun/step-3.7-flash,z-ai/glm-5.3-flash,google/gemini-3.8-flash,deepseek/deepseek-v4.1-flash
STRONG=openai/gpt-6-sol,anthropic/claude-sonnet-5.5,anthropic/claude-opus-5.5
C6=login_noquote,checkout,confirm_delete,invoice_row,spa_tabs,datepicker
H6=parcelwise_hold_overdue,orders_refund_total,laptop_most_ram_under_budget,session_expiry_resave,expense_report_submit,audit_log_revoked_key
C3=checkout,confirm_delete,spa_tabs
H3=parcelwise_hold_overdue,orders_refund_total,session_expiry_resave
run() { $B bench --suite "$1" --mode experiment --only "$2" --planner-model "$3" --label "$4" > bench/results/$4.log 2>&1; echo "done $4: $(grep -c PASS bench/results/$4.log) / $(grep -cE 'PASS|FAIL' bench/results/$4.log)"; }
run bench/scenarios.toml $C6 $CHEAP p1-comp-exp-cheap &
run bench/hard.toml      $H6 $CHEAP p1-hard-exp-cheap &
run bench/scenarios.toml $C3 $STRONG p1-comp-exp-strong &
run bench/hard.toml      $H3 $STRONG p1-hard-exp-strong &
wait
