"""Strips tracking noise from recorded InnerTube responses so parser fixtures stay small.

usage: python -I tools/trim_fixture.py <file.json>...   (rewrites in place)
"""
import json, sys

DROP = {"trackingParams", "clickTrackingParams", "loggingContext", "accessibility",
        "accessibilityData", "serviceTrackingParams", "responseContext", "loggingDirectives",
        "adSignalsInfo", "maxAgeStoreSeconds"}

def trim(v):
    if isinstance(v, dict):
        return {k: trim(x) for k, x in v.items() if k not in DROP}
    if isinstance(v, list):
        return [trim(x) for x in v]
    return v

for p in sys.argv[1:]:
    with open(p, encoding="utf-8") as f:
        v = json.load(f)
    before = len(json.dumps(v))
    v = trim(v)
    with open(p, "w", encoding="utf-8") as f:
        json.dump(v, f, ensure_ascii=False, separators=(",", ":"))
    print(f"{p}: {before // 1024} KB -> {len(json.dumps(v)) // 1024} KB")
