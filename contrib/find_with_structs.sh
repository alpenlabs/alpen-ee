#!/usr/bin/env bash

# Search for Rust type declarations (struct, type, enum) that use "With" as a connector between two concepts
# Prints matches and returns exit code 1 if any matches are found, 0 if none found

# Pattern: struct/type/enum SomethingWithSomething (where "With" connects two CamelCase words)
# This catches imprecise naming like FoodWithDrink, DataWithMetadata, etc.

root_dir=$1

if [ -z "$root_dir" ]; then
	root_dir=.
fi

matches=$(grep -r \
    --include="*.rs" \
    -E '(struct|type|enum)\s+[A-Z][a-z]+[A-Za-z0-9_]*With[A-Z][A-Za-z0-9_]*(<[^>]*>)?' \
    "$root_dir" 2>/dev/null |
    # Required associated type of upstream alloy-evm's BlockExecutor trait.
    grep -vE '(^|/)crates/reth/evm/src/config[.]rs:[[:space:]]+type TransactionResultWithState = E::TransactionResultWithState;$')

if [ -n "$matches" ]; then
    echo "found bad 'With' type declarations in path $root_dir"
    echo "$matches"
    exit 1
else
    exit 0
fi
