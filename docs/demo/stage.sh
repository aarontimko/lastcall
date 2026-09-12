#!/usr/bin/env bash
# Stage the README demo: three small repositories with agent-style uncommitted changes,
# an isolated config and state directory, and the VHS tape with this machine's paths.
#
#   docs/demo/stage.sh <demo-dir> [lastcall-binary]
#   (cd <demo-dir> && vhs lastcall.tape)
#   docs/demo/render.sh <demo-dir>
#
# The third step writes <demo-dir>/lastcall.gif; copy it over docs/demo/lastcall.gif.
# Pick a fresh <demo-dir> for every take (an accept or a flag from the last take is
# remembered in its state directory) and one outside any git work tree. Its path shows
# in the recording once, on the status line after the flag, so keep it short. Nothing
# here touches ~/.config/lastcall or ~/.local/state/lastcall: the tape exports
# LASTCALL_CONFIG and LASTCALL_STATE_DIR for its own shell only.
set -euo pipefail
demo="${1:?demo dir}"
bin="${2:-$(command -v lastcall || true)}"
[ -x "$bin" ] || { echo "stage: no lastcall binary (pass one as the second argument)" >&2; exit 2; }
here="$(cd "$(dirname "$0")" && pwd)"
[ -e "$demo" ] && { echo "stage: $demo exists; pick a fresh directory" >&2; exit 2; }
mkdir -p "$demo/repos" "$demo/state"
# Inside another repository's work tree, lastcall would find that repository, not these.
if git -C "$demo" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    echo "stage: $demo is inside a git work tree; pick a directory that is not" >&2; exit 2
fi

repo() { # repo <name>: a repository with one baseline commit
    mkdir -p "$demo/repos/$1"
    git -C "$demo/repos/$1" init -q -b main
    git -C "$demo/repos/$1" config user.name "demo"
    git -C "$demo/repos/$1" config user.email "demo"
}
commit() { git -C "$demo/repos/$1" add -A && git -C "$demo/repos/$1" commit -q -m "$2"; }

# api: a Python service. The agent fixed a bug and added a test.
repo api
cat > "$demo/repos/api/orders.py" <<'PY'
from dataclasses import dataclass


@dataclass
class Order:
    id: int
    total_cents: int
    coupon: str | None = None


def apply_coupon(order: Order, coupons: dict[str, int]) -> int:
    if order.coupon is None:
        return order.total_cents
    discount = coupons[order.coupon]
    return order.total_cents - discount


def summarize(orders: list[Order]) -> str:
    total = sum(o.total_cents for o in orders)
    return f"{len(orders)} orders, {total / 100:.2f} total"
PY
cat > "$demo/repos/api/test_orders.py" <<'PY'
from orders import Order, apply_coupon


def test_no_coupon():
    assert apply_coupon(Order(1, 1000), {}) == 1000
PY
commit api "orders: coupons and the summary line"
cat > "$demo/repos/api/orders.py" <<'PY'
from dataclasses import dataclass


@dataclass
class Order:
    id: int
    total_cents: int
    coupon: str | None = None


def apply_coupon(order: Order, coupons: dict[str, int]) -> int:
    if order.coupon is None:
        return order.total_cents
    discount = coupons.get(order.coupon, 0)
    return max(order.total_cents - discount, 0)


def summarize(orders: list[Order]) -> str:
    total = sum(o.total_cents for o in orders)
    return f"{len(orders)} orders, {total / 100:.2f} total"
PY
cat >> "$demo/repos/api/test_orders.py" <<'PY'


def test_unknown_coupon_is_ignored():
    assert apply_coupon(Order(2, 1000, "NOPE"), {}) == 1000


def test_discount_never_goes_negative():
    assert apply_coupon(Order(3, 500, "BIG"), {"BIG": 900}) == 0
PY

# web: a small TypeScript client. The agent renamed a helper and touched the lockfile.
repo web
mkdir -p "$demo/repos/web/src"
cat > "$demo/repos/web/src/format.ts" <<'TS'
export function money(cents: number): string {
  return (cents / 100).toFixed(2);
}

export function label(count: number): string {
  return count === 1 ? "1 order" : `${count} orders`;
}
TS
printf '{\n  "name": "web",\n  "lockfileVersion": 3,\n  "packages": {}\n}\n' > "$demo/repos/web/package-lock.json"
commit web "web: formatting helpers"
cat > "$demo/repos/web/src/format.ts" <<'TS'
const formatter = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD" });

export function money(cents: number): string {
  return formatter.format(cents / 100);
}

export function label(count: number): string {
  return count === 1 ? "1 order" : `${count} orders`;
}
TS
printf '{\n  "name": "web",\n  "lockfileVersion": 3,\n  "packages": {\n    "node_modules/left-pad": { "version": "1.3.0" }\n  }\n}\n' > "$demo/repos/web/package-lock.json"

# infra: nothing pending, so the list shows a quiet repository too.
repo infra
printf 'region = "us-east-1"\n' > "$demo/repos/infra/main.tf"
commit infra "infra: the region"

cat > "$demo/config.toml" <<TOML
parent_dirs = ["$demo/repos"]

[herdr]
mode = "off"
TOML

sed -e "s|@DEMO@|$demo|g" -e "s|@BIN_DIR@|$(cd "$(dirname "$bin")" && pwd)|g" \
    "$here/lastcall.tape.in" > "$demo/lastcall.tape"
echo "staged $demo"
echo "record: (cd $demo && vhs lastcall.tape)"
echo "encode: $here/render.sh $demo   (writes $demo/lastcall.gif)"
