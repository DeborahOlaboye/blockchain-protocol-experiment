#!/usr/bin/env bash
set -euo pipefail

DEMO_DIR="$(cd "$(dirname "$0")" && pwd)"
NODE_DIR="$(dirname "$DEMO_DIR")"
RPC_URL="${RPC_URL:-http://127.0.0.1:8545}"
ALICE_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
BOB_KEY="0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
ALICE="$(cast wallet address --private-key "$ALICE_KEY")"
BOB="$(cast wallet address --private-key "$BOB_KEY")"
NODE_PID=""

stop_node() {
  if [ -n "$NODE_PID" ]; then
    kill "$NODE_PID" 2>/dev/null || true
  fi
}
trap stop_node EXIT

send() {
  cast send --legacy --rpc-url "$RPC_URL" --json "$@"
}

field() {
  jq -r ".$1"
}

eth() {
  cast from-wei "$1"
}

if ! cast chain-id --rpc-url "$RPC_URL" >/dev/null 2>&1; then
  LOG="$(mktemp -t breeja-demo)"
  echo "== No node at $RPC_URL, starting one with --dev (log: $LOG)"
  (cd "$NODE_DIR" && cargo build --release --quiet)
  "$NODE_DIR/target/release/breeja" --dev --rpc-address "${RPC_URL#http://}" >"$LOG" 2>&1 &
  NODE_PID=$!
  until cast chain-id --rpc-url "$RPC_URL" >/dev/null 2>&1; do sleep 0.2; done
fi

if command -v solc >/dev/null 2>&1; then
  echo "== Compiling Vault.sol with solc $(solc --version | tail -1 | awk '{print $2}' | cut -d+ -f1) for Shanghai"
  BYTECODE="0x$(solc --evm-version shanghai --optimize --bin "$DEMO_DIR/Vault.sol" 2>/dev/null | tail -1)"
else
  echo "== solc not found, using the precompiled demo/Vault.bin"
  BYTECODE="$(cat "$DEMO_DIR/Vault.bin")"
fi

echo "== Deploying Vault from Alice ($ALICE)"
DEPLOY="$(send --private-key "$ALICE_KEY" --create "$BYTECODE")"
VAULT="$(echo "$DEPLOY" | field contractAddress)"
echo "   address $VAULT, block $(cast to-dec "$(echo "$DEPLOY" | field blockNumber)"), gas $(cast to-dec "$(echo "$DEPLOY" | field gasUsed)")"
echo "   eth_getCode: $(( ($(cast code --rpc-url "$RPC_URL" "$VAULT" | wc -c) - 3) / 2 )) bytes of runtime code"

echo "== Alice deposits 2 ETH, Bob deposits 1 ETH"
for who in "$ALICE_KEY 2ether" "$BOB_KEY 1ether"; do
  set -- $who
  TX="$(send --private-key "$1" --value "$2" "$VAULT" 'deposit()')"
  echo "   status $(echo "$TX" | field status), gas $(cast to-dec "$(echo "$TX" | field gasUsed)"), $(echo "$TX" | jq '.logs | length') Deposited event"
done
echo "   ETH held by the contract: $(eth "$(cast balance --rpc-url "$RPC_URL" "$VAULT")")"

echo "== Alice withdraws 0.5 ETH (the contract sends ETH back with an internal CALL)"
BEFORE="$(cast balance --rpc-url "$RPC_URL" "$VAULT")"
TX="$(send --private-key "$ALICE_KEY" "$VAULT" 'withdraw(uint256)' 0.5ether)"
AFTER="$(cast balance --rpc-url "$RPC_URL" "$VAULT")"
echo "   status $(echo "$TX" | field status), gas $(cast to-dec "$(echo "$TX" | field gasUsed)")"
echo "   Withdrawn event amount: $(eth "$(cast to-dec "$(echo "$TX" | jq -r '.logs[0].data')")") ETH"
echo "   ETH held by the contract: $(eth "$BEFORE") -> $(eth "$AFTER")"

echo "== Reading state back"
echo "   balanceOf(Alice)   $(eth "$(cast call --rpc-url "$RPC_URL" "$VAULT" 'balanceOf(address)(uint256)' "$ALICE" | awk '{print $1}')") ETH"
echo "   balanceOf(Bob)     $(eth "$(cast call --rpc-url "$RPC_URL" "$VAULT" 'balanceOf(address)(uint256)' "$BOB" | awk '{print $1}')") ETH"
echo "   totalDeposits      $(eth "$(cast call --rpc-url "$RPC_URL" "$VAULT" 'totalDeposits()(uint256)' | awk '{print $1}')") ETH"
echo "   eth_getStorageAt slot 1 (totalDeposits):        $(cast storage --rpc-url "$RPC_URL" "$VAULT" 1)"
echo "   eth_getStorageAt keccak(Alice . 0) (her entry): $(cast storage --rpc-url "$RPC_URL" "$VAULT" "$(cast index address "$ALICE" 0)")"

echo "== eth_getLogs: every Deposited event, then only Alice's"
cast logs --rpc-url "$RPC_URL" --from-block 0 --to-block latest --address "$VAULT" \
  'Deposited(address indexed,uint256)' --json \
  | jq -r '.[] | "   all:   block \(.blockNumber) account 0x\(.topics[1][26:]) amount \(.data)"'
cast logs --rpc-url "$RPC_URL" --from-block 0 --to-block latest --address "$VAULT" \
  'Deposited(address indexed,uint256)' "$ALICE" --json \
  | jq -r '.[] | "   Alice: block \(.blockNumber) account 0x\(.topics[1][26:]) amount \(.data)"'

echo "== eth_estimateGas for withdraw(0.1 ETH): $(cast estimate --rpc-url "$RPC_URL" --from "$ALICE" "$VAULT" 'withdraw(uint256)' 0.1ether)"

echo "== eth_call withdraw(5 ETH): the custom error comes back as revert data"
cast call --rpc-url "$RPC_URL" --from "$ALICE" "$VAULT" 'withdraw(uint256)' 5ether 2>&1 | sed 's/^/   /' || true
REVERT_DATA="$(cast call --rpc-url "$RPC_URL" --from "$ALICE" "$VAULT" 'withdraw(uint256)' 5ether 2>&1 | grep -o '0xcf479181[0-9a-f]*' || true)"
DECODED="$(cast decode-error --sig 'InsufficientBalance(uint256,uint256)' "$REVERT_DATA" | awk '{print $1}')"
echo "   decoded: InsufficientBalance(available $(eth "$(echo "$DECODED" | sed -n 1p)") ETH, requested $(eth "$(echo "$DECODED" | sed -n 2p)") ETH)"

echo "== Sending the same failing withdrawal as a real transaction"
NONCE="$(cast nonce --rpc-url "$RPC_URL" "$ALICE")"
TX="$(send --private-key "$ALICE_KEY" --gas-limit 100000 "$VAULT" 'withdraw(uint256)' 5ether 2>/dev/null || true)"
HASH="$(echo "$TX" | field transactionHash)"
RECEIPT="$(cast receipt --rpc-url "$RPC_URL" --json "$HASH")"
echo "   mined in block $(cast to-dec "$(echo "$RECEIPT" | field blockNumber)") with status $(echo "$RECEIPT" | field status), gas $(cast to-dec "$(echo "$RECEIPT" | field gasUsed)") still charged"
echo "   Alice's nonce went from $NONCE to $(cast nonce --rpc-url "$RPC_URL" "$ALICE")"
