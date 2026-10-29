"""Account context shared by storage and transport; independent of HTTP and service layers."""

import contextlib
import contextvars
from collections.abc import Awaitable, Callable, Iterator

Policy = Callable[[str, dict], Awaitable[dict]]
_user: contextvars.ContextVar[int | None] = contextvars.ContextVar("account", default=None)
_policy: contextvars.ContextVar[Policy | None] = contextvars.ContextVar("telegram_policy", default=None)


def user_id() -> int | None:
    return _user.get()


@contextlib.contextmanager
def bind(uid: int | None, policy: Policy | None = None) -> Iterator[None]:
    user, guard = _user.set(uid), _policy.set(policy)
    try:
        yield
    finally:
        _policy.reset(guard)
        _user.reset(user)


async def authorize(method: str, params: dict) -> dict:
    policy = _policy.get()
    if user_id() is not None and policy is None:
        raise RuntimeError("account transport policy is missing")
    return await policy(method, params) if policy else params
