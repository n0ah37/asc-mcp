#!/usr/bin/env python3
"""Rebuild spec/asc-openapi.json.gz from Apple's App Store Connect OpenAPI spec.

Usage: scripts/update_spec.py [path-to-openapi.oas.json]
With no argument it downloads Apple's current spec zip.

The output keeps what api_search, api_describe and api_execute need (every
operation's method, path, tags, parameters and request/response schema names,
plus all component schemas) and drops prose and error responses, which cuts
the embedded file to a fraction of the original.
"""
import gzip
import io
import json
import pathlib
import sys
import urllib.request
import zipfile

URL = "https://developer.apple.com/sample-code/app-store-connect/app-store-connect-openapi-specification.zip"
OUT = pathlib.Path(__file__).resolve().parent.parent / "spec" / "asc-openapi.json.gz"
METHODS = ("get", "post", "patch", "put", "delete")


def load(argv):
    if len(argv) > 1:
        return json.load(open(argv[1]))
    data = urllib.request.urlopen(URL, timeout=60).read()
    with zipfile.ZipFile(io.BytesIO(data)) as z:
        name = next(n for n in z.namelist() if n.endswith(".json"))
        return json.loads(z.read(name))


def strip(node):
    if isinstance(node, dict):
        # Prose keys hold strings; an attribute that happens to be *named*
        # "description" or "title" holds a schema object and is kept.
        return {
            k: strip(v)
            for k, v in node.items()
            if not (k in ("description", "example", "title") and not isinstance(v, dict))
            and k != "examples"
        }
    if isinstance(node, list):
        return [strip(v) for v in node]
    return node


def ref_name(schema):
    if not schema:
        return None
    ref = schema.get("$ref") or (schema.get("items") or {}).get("$ref")
    return ref.rsplit("/", 1)[-1] if ref else None


def main(argv):
    spec = load(argv)
    ops = []
    for path, item in spec["paths"].items():
        shared = item.get("parameters", [])
        for method in METHODS:
            op = item.get(method)
            if not op:
                continue
            params = []
            for p in shared + op.get("parameters", []):
                if p.get("in") == "path":
                    continue
                params.append({"name": p["name"], "required": bool(p.get("required")), "schema": strip(p.get("schema", {}))})
            body = (op.get("requestBody") or {}).get("content", {}).get("application/json", {}).get("schema")
            response = None
            for status, resp in op.get("responses", {}).items():
                if status.startswith("2"):
                    response = ref_name((resp.get("content") or {}).get("application/json", {}).get("schema")) or status
                    break
            ops.append({
                "id": op.get("operationId", ""),
                "method": method.upper(),
                "path": path,
                "tags": op.get("tags", []),
                "deprecated": bool(op.get("deprecated")),
                "params": params,
                "body": ref_name(body),
                "response": response,
            })
    out = {
        "version": spec["info"]["version"],
        "operations": ops,
        "schemas": strip(spec["components"]["schemas"]),
    }
    raw = json.dumps(out, separators=(",", ":"), sort_keys=True).encode()
    OUT.parent.mkdir(exist_ok=True)
    # mtime=0 keeps the file byte-identical across rebuilds of the same spec.
    with open(OUT, "wb") as f:
        with gzip.GzipFile(fileobj=f, mode="wb", compresslevel=9, mtime=0) as gz:
            gz.write(raw)
    print(f"API {out['version']}: {len(ops)} operations, {len(out['schemas'])} schemas, {OUT.stat().st_size} bytes")


if __name__ == "__main__":
    main(sys.argv)
