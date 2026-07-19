"""Per-action manual-verification gate, hardened against model spoofing/delegation.

The gate decision is **escalate-only** so a self-reported model can never *downgrade* policy:

  - `GDRIVE_MCP_REQUIRE_VERIFICATION=always` (operator-set at launch) gates every call.
    This is the anti-delegation lever: a caller routed through the same server cannot escape it.
  - otherwise, a call is gated when either the operator pin `GDRIVE_MCP_CALLING_MODEL` or the
    request's self-reported `_meta.model` matches a case-insensitive substring from the
    comma-separated `GDRIVE_MCP_VERIFICATION_MODEL_PATTERNS` setting. A nonmatching request
    model cannot turn off a match from the operator pin.

When gated, every action needs per-call user approval via MCP elicitation. It fails closed on
decline or if the client cannot prompt. `_meta.model` is advisory only; deployment-level policy
should use the operator-controlled environment variables.
"""

from __future__ import annotations

import inspect
import os

from mcp.server.fastmcp import Context
from pydantic import BaseModel, Field

from gdrive_mcp import audit
from gdrive_mcp.clients import authed_user_email

_TRUTHY = {"always", "1", "true", "yes", "on"}


class _Approval(BaseModel):
    approved: bool = Field(description="Approve this verification-gated Google Drive action?")


def _normalize_model(value: str) -> str:
    return value.strip().lower().replace("_", "-")


def _verification_model_patterns() -> tuple[str, ...]:
    raw = os.environ.get("GDRIVE_MCP_VERIFICATION_MODEL_PATTERNS", "")
    return tuple(_normalize_model(part) for part in raw.split(",") if part.strip())


def _model_requires_verification(model: str | None) -> bool:
    if not model:
        return False
    normalized = _normalize_model(model)
    return any(pattern in normalized for pattern in _verification_model_patterns())


def _meta_model(ctx: Context | None) -> str | None:
    meta = getattr(getattr(ctx, "request_context", None), "meta", None)
    return getattr(meta, "model", None) if meta else None


def calling_model(ctx: Context | None) -> str | None:
    """Best-known caller model, for display only (advisory; never trusted to downgrade)."""
    return _meta_model(ctx) or os.environ.get("GDRIVE_MCP_CALLING_MODEL")


def gate_required(ctx: Context | None) -> bool:
    """Whether this call must be user-verified. Escalate-only + operator override."""
    if os.environ.get("GDRIVE_MCP_REQUIRE_VERIFICATION", "").strip().lower() in _TRUTHY:
        return True
    return _model_requires_verification(
        os.environ.get("GDRIVE_MCP_CALLING_MODEL")
    ) or _model_requires_verification(_meta_model(ctx))


async def require_verification(ctx: Context | None, tool_name: str, args: dict) -> None:
    """Block until the user approves when the call is gated; no-op otherwise. Fails closed."""
    if not gate_required(ctx):
        return
    if ctx is None:
        raise RuntimeError(
            f"'{tool_name}' needs manual user verification but no request context is available; "
            f"action blocked."
        )
    target = args.get("item") or args.get("name") or args.get("title") or "the requested target"
    model = calling_model(ctx) or "unknown"
    try:
        result = await ctx.elicit(
            message=f"Approve `{tool_name}` on {target}? (verification-gated; caller model: {model})",
            schema=_Approval,
        )
    except Exception as exc:
        raise RuntimeError(
            f"'{tool_name}' requires manual user verification but the client cannot prompt the "
            f"user ({type(exc).__name__}); action blocked."
        )
    if result.action != "accept" or not getattr(result.data, "approved", False):
        raise RuntimeError(f"'{tool_name}' was not approved by the user; action blocked.")


def gated(fn):
    """Wrap a tool so it runs require_verification (via an injected Context) before executing.

    The original signature is resolved (eval_str) and a keyword-only `ctx: Context` is appended,
    which FastMCP injects and excludes from the tool's input schema.
    """
    resolved = inspect.signature(fn, eval_str=True)
    params = [*resolved.parameters.values(), inspect.Parameter("ctx", inspect.Parameter.KEYWORD_ONLY, annotation=Context)]

    async def wrapper(**kwargs):
        ctx = kwargs.pop("ctx", None)
        try:
            await require_verification(ctx, fn.__name__, kwargs)
            result = fn(**kwargs)
        except Exception as exc:
            audit.record(fn.__name__, kwargs, f"error:{type(exc).__name__}", user=authed_user_email())
            raise
        audit.record(fn.__name__, kwargs, "ok", user=authed_user_email())
        return result

    wrapper.__name__ = fn.__name__
    wrapper.__doc__ = fn.__doc__
    wrapper.__signature__ = resolved.replace(parameters=params)
    wrapper.__annotations__ = {
        p.name: p.annotation for p in params if p.annotation is not inspect.Parameter.empty
    }
    return wrapper
