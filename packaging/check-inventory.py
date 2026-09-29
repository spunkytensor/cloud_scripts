#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Matt Curfman
# SPDX-License-Identifier: Apache-2.0
"""Require the source inventory to contain every external locked Rust package."""

import json
import sys
import tomllib


def reconcile(lock, report):
    expected = {(p["name"], p["version"]) for p in lock["package"] if "source" in p}
    actual = {
        (p["Name"], p["Version"])
        for result in report["Results"]
        if result.get("Type") == "cargo"
        for p in result.get("Packages", [])
    }
    if not expected or expected - actual:
        raise ValueError(f"Empty lockfile or missing locked packages: {sorted(expected - actual)}")
    return len(expected)


if __name__ == "__main__":
    with open(sys.argv[1], "rb") as lockfile, open(sys.argv[2]) as report:
        count = reconcile(tomllib.load(lockfile), json.load(report))
    print(f"Inventory covers all {count} external locked Rust package versions")
