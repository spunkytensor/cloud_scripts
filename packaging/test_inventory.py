# SPDX-FileCopyrightText: 2026 Matt Curfman
# SPDX-License-Identifier: Apache-2.0

import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("inventory", Path(__file__).with_name("check-inventory.py"))
inventory = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inventory)


class InventoryTests(unittest.TestCase):
    def setUp(self):
        self.lock = {"package": [
            {"name": "vps", "version": "0.1.0"},
            {"name": "crate", "version": "1.0.0", "source": "registry"},
            {"name": "crate", "version": "2.0.0", "source": "registry"},
        ]}
        self.report = {"Results": [{"Type": "cargo", "Packages": [
            {"Name": "crate", "Version": "1.0.0"},
            {"Name": "crate", "Version": "2.0.0"},
        ]}]}

    def test_complete_inventory_excludes_local_package(self):
        self.assertEqual(inventory.reconcile(self.lock, self.report), 2)

    def test_missing_version_fails(self):
        self.report["Results"][0]["Packages"].pop()
        with self.assertRaises(ValueError):
            inventory.reconcile(self.lock, self.report)

    def test_wrong_ecosystem_fails(self):
        self.report["Results"][0]["Type"] = "npm"
        with self.assertRaises(ValueError):
            inventory.reconcile(self.lock, self.report)

    def test_empty_inventory_fails(self):
        with self.assertRaises(ValueError):
            inventory.reconcile(self.lock, {"Results": []})

    def test_empty_lock_fails(self):
        with self.assertRaises(ValueError):
            inventory.reconcile({"package": []}, self.report)

    def test_malformed_report_fails(self):
        with self.assertRaises(KeyError):
            inventory.reconcile(self.lock, {})
