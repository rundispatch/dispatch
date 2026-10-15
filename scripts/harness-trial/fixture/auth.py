class AuthError(Exception):
    pass


def validate(token):
    if not token:
        raise AuthError("missing token")
    return {"user": token}
