import unittest

import admin
from auth import AuthError, validate


class AuthTest(unittest.TestCase):
    def test_validate_returns_the_user(self):
        self.assertEqual(validate("t")["user"], "t")

    def test_validate_refuses_a_missing_token(self):
        with self.assertRaises(AuthError):
            validate("")


class AdminTest(unittest.TestCase):
    def test_promote(self):
        self.assertEqual(admin.promote("t"), {"promoted": True})


if __name__ == "__main__":
    unittest.main()
