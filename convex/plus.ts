/**
 * HexaTalk Plus — cosmetic subscription (badge, banner, extended profile
 * cosmetics). Intentionally NOT pay-to-win: no voice quality, permissions,
 * server power, or exclusive competitive advantages.
 *
 * Lifecycle:
 *  1. Client calls `createCheckoutSession` (action) → Stripe Checkout URL
 *  2. User pays in browser
 *  3. Stripe POSTs to `/stripe/webhook` (convex/http.ts)
 *  4. Webhook patches `users.plusExpiresAt` / Stripe ids
 *  5. Clients read `plusActive` from auth:me / profile / messages / members
 *
 * Env (Convex dashboard → Settings → Environment Variables):
 *  - STRIPE_SECRET_KEY
 *  - STRIPE_WEBHOOK_SECRET
 *  - STRIPE_PRICE_ID          (recurring Price id for Plus)
 *  - STRIPE_SUCCESS_URL       (default: https://buy.vyrapp.pro/success)
 *  - STRIPE_CANCEL_URL        (default: https://buy.vyrapp.pro/cancel)
 *
 * Public purchase landing: https://buy.vyrapp.pro
 */
import { v } from "convex/values";
import {
  action,
  internalMutation,
  internalQuery,
  mutation,
  query,
} from "./_generated/server";
import { internal } from "./_generated/api";
import { Doc, Id } from "./_generated/dataModel";
import { currentUser, requireStaff } from "./session";

// No @types/node in this project; Convex runtime provides process.env.
declare const process: { env: Record<string, string | undefined> };

export const FREE_STATUS_MAX = 100;
export const FREE_BIO_MAX = 300;
export const PLUS_STATUS_MAX = 150;
export const PLUS_BIO_MAX = 500;
export const MAX_BANNER_BYTES = 4 * 1024 * 1024;

const HEX_COLOR = /^#[0-9A-Fa-f]{6}$/;

/** True when the user currently has HexaTalk Plus. */
export function isPlusActive(user: Doc<"users">): boolean {
  const exp = user.plusExpiresAt;
  return typeof exp === "number" && exp > Date.now();
}

export function plusPublicFields(user: Doc<"users">) {
  const active = isPlusActive(user);
  return {
    plusActive: active,
    plusExpiresAt: active ? (user.plusExpiresAt as number) : 0,
  };
}

export function isValidHexColor(color: string): boolean {
  return HEX_COLOR.test(color);
}

// ---------- Public status ----------

export const getMyStatus = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const active = isPlusActive(me);
    return {
      active,
      expiresAt: active ? (me.plusExpiresAt as number) : 0,
      hasStripeCustomer: !!me.stripeCustomerId,
      // Marketing / UI copy — not pay-to-win.
      benefits: [
        "PLUS badge next to your name",
        "Custom profile banner",
        "Any avatar color (hex)",
        "Longer status message & bio",
      ],
    };
  },
});

// ---------- Admin (manual grant / revoke for tests & gifts) ----------

export const adminGrant = mutation({
  args: {
    sessionToken: v.string(),
    userId: v.id("users"),
    /** Days of Plus from now (default 30). */
    days: v.optional(v.number()),
  },
  handler: async (ctx, args) => {
    await requireStaff(ctx, args.sessionToken);
    const target = await ctx.db.get("users", args.userId);
    if (!target) throw new Error("User not found");
    if (target.isBot) throw new Error("Bots can't have Plus");

    const days = Math.max(1, Math.min(args.days ?? 30, 3650));
    const now = Date.now();
    const base =
      typeof target.plusExpiresAt === "number" && target.plusExpiresAt > now
        ? target.plusExpiresAt
        : now;
    const plusExpiresAt = base + days * 24 * 60 * 60 * 1000;
    await ctx.db.patch("users", target._id, { plusExpiresAt });
    return { plusExpiresAt };
  },
});

export const adminRevoke = mutation({
  args: {
    sessionToken: v.string(),
    userId: v.id("users"),
  },
  handler: async (ctx, args) => {
    await requireStaff(ctx, args.sessionToken);
    const target = await ctx.db.get("users", args.userId);
    if (!target) throw new Error("User not found");
    await ctx.db.patch("users", target._id, {
      plusExpiresAt: undefined,
      // Keep Stripe ids so re-subscribe reuses the same customer.
    });
    return null;
  },
});

// ---------- Profile banner (Plus-only) ----------

export const generateBannerUploadUrl = mutation({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (!isPlusActive(me)) {
      throw new Error("HexaTalk Plus is required for a profile banner");
    }
    return await ctx.storage.generateUploadUrl();
  },
});

export const setProfileBanner = mutation({
  args: {
    sessionToken: v.string(),
    storageId: v.id("_storage"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (!isPlusActive(me)) {
      throw new Error("HexaTalk Plus is required for a profile banner");
    }

    const metadata = await ctx.db.system.get("_storage", args.storageId);
    if (
      !metadata ||
      metadata.size > MAX_BANNER_BYTES ||
      (metadata.contentType !== undefined &&
        !metadata.contentType.startsWith("image/"))
    ) {
      await ctx.storage.delete(args.storageId);
      throw new Error("Banner must be an image smaller than 4MB");
    }

    const previousId = me.profileBannerStorageId;
    await ctx.db.patch("users", me._id, {
      profileBannerStorageId: args.storageId,
    });
    if (previousId && previousId !== args.storageId) {
      await ctx.storage.delete(previousId);
    }

    const url = await ctx.storage.getUrl(args.storageId);
    return url ?? "";
  },
});

export const removeProfileBanner = mutation({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (me.profileBannerStorageId) {
      await ctx.storage.delete(me.profileBannerStorageId);
      await ctx.db.patch("users", me._id, {
        profileBannerStorageId: undefined,
      });
    }
    return null;
  },
});

// ---------- Stripe helpers (internal) ----------

export const getUserById = internalQuery({
  args: { userId: v.id("users") },
  handler: async (ctx, args) => {
    return await ctx.db.get("users", args.userId);
  },
});

export const getUserByStripeCustomerId = internalQuery({
  args: { stripeCustomerId: v.string() },
  handler: async (ctx, args) => {
    return await ctx.db
      .query("users")
      .withIndex("by_stripeCustomerId", (q) =>
        q.eq("stripeCustomerId", args.stripeCustomerId),
      )
      .unique();
  },
});

export const getUserByStripeSubscriptionId = internalQuery({
  args: { stripeSubscriptionId: v.string() },
  handler: async (ctx, args) => {
    return await ctx.db
      .query("users")
      .withIndex("by_stripeSubscriptionId", (q) =>
        q.eq("stripeSubscriptionId", args.stripeSubscriptionId),
      )
      .unique();
  },
});

export const markStripeEventProcessed = internalMutation({
  args: { eventId: v.string() },
  handler: async (ctx, args) => {
    const existing = await ctx.db
      .query("stripeEvents")
      .withIndex("by_eventId", (q) => q.eq("eventId", args.eventId))
      .unique();
    if (existing) return { already: true as const };
    await ctx.db.insert("stripeEvents", {
      eventId: args.eventId,
      processedAt: Date.now(),
    });
    return { already: false as const };
  },
});

export const applyPlusEntitlement = internalMutation({
  args: {
    userId: v.id("users"),
    plusExpiresAt: v.optional(v.number()),
    stripeCustomerId: v.optional(v.string()),
    stripeSubscriptionId: v.optional(v.string()),
    clearPlus: v.optional(v.boolean()),
  },
  handler: async (ctx, args) => {
    const user = await ctx.db.get("users", args.userId);
    if (!user) return null;

    const patch: Partial<Doc<"users">> = {};
    if (args.stripeCustomerId) {
      patch.stripeCustomerId = args.stripeCustomerId;
    }
    if (args.stripeSubscriptionId) {
      patch.stripeSubscriptionId = args.stripeSubscriptionId;
    }
    if (args.clearPlus) {
      patch.plusExpiresAt = undefined;
    } else if (typeof args.plusExpiresAt === "number") {
      patch.plusExpiresAt = args.plusExpiresAt;
    }

    if (Object.keys(patch).length > 0) {
      await ctx.db.patch("users", user._id, patch);
    }
    return null;
  },
});

// ---------- Stripe Checkout / Portal (actions) ----------

function requireEnv(name: string): string {
  const v = process.env[name];
  if (!v) {
    throw new Error(
      `${name} is not configured. Set it in the Convex dashboard (Environment Variables).`,
    );
  }
  return v;
}

async function stripeForm(
  path: string,
  params: Record<string, string>,
): Promise<Record<string, unknown>> {
  const secret = requireEnv("STRIPE_SECRET_KEY");
  const body = new URLSearchParams(params);
  const res = await fetch(`https://api.stripe.com/v1${path}`, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${secret}`,
      "Content-Type": "application/x-www-form-urlencoded",
    },
    body: body.toString(),
  });
  const json = (await res.json()) as Record<string, unknown>;
  if (!res.ok) {
    const err = json.error as { message?: string } | undefined;
    throw new Error(err?.message ?? `Stripe error (${res.status})`);
  }
  return json;
}

/**
 * Opens Stripe Checkout for a monthly Plus subscription.
 * Returns `{ url }` — open it in the system browser.
 */
export const createCheckoutSession = action({
  args: {
    sessionToken: v.string(),
  },
  handler: async (ctx, args): Promise<{ url: string }> => {
    const me = await ctx.runQuery(internal.plus.resolveSessionUser, {
      sessionToken: args.sessionToken,
    });
    if (!me) throw new Error("Session expired, please log in again");
    if (me.isBot) throw new Error("Bots can't subscribe");

    if (isPlusActive(me as Doc<"users">) && me.stripeCustomerId) {
      // Already on a Stripe sub — Customer Portal to manage / cancel.
      const portal = await stripeForm("/billing_portal/sessions", {
        customer: me.stripeCustomerId,
        return_url:
          process.env.STRIPE_SUCCESS_URL ?? "https://buy.vyrapp.pro/success",
      });
      const url = portal.url as string | undefined;
      if (!url) throw new Error("Stripe portal did not return a URL");
      return { url };
    }

    const priceId = requireEnv("STRIPE_PRICE_ID");
    const successUrl =
      process.env.STRIPE_SUCCESS_URL ??
      "https://buy.vyrapp.pro/success?session_id={CHECKOUT_SESSION_ID}";
    const cancelUrl =
      process.env.STRIPE_CANCEL_URL ?? "https://buy.vyrapp.pro/cancel";

    const params: Record<string, string> = {
      mode: "subscription",
      "line_items[0][price]": priceId,
      "line_items[0][quantity]": "1",
      success_url: successUrl,
      cancel_url: cancelUrl,
      client_reference_id: me._id,
      "metadata[userId]": me._id,
      "subscription_data[metadata][userId]": me._id,
    };
    if (me.email) {
      params.customer_email = me.email;
    }
    if (me.stripeCustomerId) {
      params.customer = me.stripeCustomerId;
      delete params.customer_email;
    }

    const session = await stripeForm("/checkout/sessions", params);
    const url = session.url as string | undefined;
    if (!url) throw new Error("Stripe Checkout did not return a URL");
    return { url };
  },
});

/**
 * Stripe Customer Portal — cancel / update payment method.
 */
export const createBillingPortal = action({
  args: { sessionToken: v.string() },
  handler: async (ctx, args): Promise<{ url: string }> => {
    const me = await ctx.runQuery(internal.plus.resolveSessionUser, {
      sessionToken: args.sessionToken,
    });
    if (!me) throw new Error("Session expired, please log in again");
    if (!me.stripeCustomerId) {
      throw new Error("No Stripe customer on this account yet — subscribe first");
    }
    const returnUrl =
      process.env.STRIPE_SUCCESS_URL ?? "https://buy.vyrapp.pro/success";
    const portal = await stripeForm("/billing_portal/sessions", {
      customer: me.stripeCustomerId,
      return_url: returnUrl,
    });
    const url = portal.url as string | undefined;
    if (!url) throw new Error("Stripe portal did not return a URL");
    return { url };
  },
});

/** Lightweight session→user for actions (mirrors auth.resolveSessionUser). */
export const resolveSessionUser = internalQuery({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    try {
      return await currentUser(ctx, args.sessionToken);
    } catch {
      return null;
    }
  },
});

// ---------- Webhook processing (called from http.ts) ----------

type StripeSubLike = {
  id?: string;
  customer?: string;
  status?: string;
  current_period_end?: number;
  metadata?: { userId?: string };
};

/**
 * Apply a verified Stripe event. Idempotent on `eventId`.
 * Returns whether the event was newly processed.
 */
export const processStripeEvent = internalMutation({
  args: {
    eventId: v.string(),
    type: v.string(),
    // JSON-encoded data.object (avoids deep Convex validators).
    objectJson: v.string(),
  },
  handler: async (ctx, args) => {
    const existing = await ctx.db
      .query("stripeEvents")
      .withIndex("by_eventId", (q) => q.eq("eventId", args.eventId))
      .unique();
    if (existing) return { ok: true, duplicate: true };

    let obj: Record<string, unknown>;
    try {
      obj = JSON.parse(args.objectJson) as Record<string, unknown>;
    } catch {
      throw new Error("Invalid Stripe event payload");
    }

    await ctx.db.insert("stripeEvents", {
      eventId: args.eventId,
      processedAt: Date.now(),
    });

    const type = args.type;

    if (type === "checkout.session.completed") {
      const userIdRaw =
        (obj.client_reference_id as string | undefined) ||
        ((obj.metadata as { userId?: string } | undefined)?.userId ?? "");
      const customerId = (obj.customer as string | undefined) ?? "";
      const subscriptionId = (obj.subscription as string | undefined) ?? "";
      if (userIdRaw) {
        const user = await ctx.db.get("users", userIdRaw as Id<"users">);
        if (user) {
          // Grant ~35 days until subscription.updated fills exact period end.
          const plusExpiresAt = Date.now() + 35 * 24 * 60 * 60 * 1000;
          await ctx.db.patch("users", user._id, {
            plusExpiresAt,
            ...(customerId ? { stripeCustomerId: customerId } : {}),
            ...(subscriptionId
              ? { stripeSubscriptionId: subscriptionId }
              : {}),
          });
        }
      }
      return { ok: true, duplicate: false };
    }

    if (
      type === "customer.subscription.updated" ||
      type === "customer.subscription.created"
    ) {
      const sub = obj as StripeSubLike;
      const userIdFromMeta = sub.metadata?.userId;
      let user: Doc<"users"> | null = null;
      if (userIdFromMeta) {
        user = await ctx.db.get("users", userIdFromMeta as Id<"users">);
      }
      if (!user && sub.id) {
        user = await ctx.db
          .query("users")
          .withIndex("by_stripeSubscriptionId", (q) =>
            q.eq("stripeSubscriptionId", sub.id),
          )
          .unique();
      }
      if (!user && sub.customer) {
        user = await ctx.db
          .query("users")
          .withIndex("by_stripeCustomerId", (q) =>
            q.eq("stripeCustomerId", sub.customer),
          )
          .unique();
      }
      if (user) {
        const status = sub.status ?? "";
        const activeStatuses = new Set([
          "active",
          "trialing",
          "past_due",
        ]);
        const periodEndMs =
          typeof sub.current_period_end === "number"
            ? sub.current_period_end * 1000
            : Date.now() + 35 * 24 * 60 * 60 * 1000;
        await ctx.db.patch("users", user._id, {
          ...(sub.customer ? { stripeCustomerId: sub.customer } : {}),
          ...(sub.id ? { stripeSubscriptionId: sub.id } : {}),
          plusExpiresAt: activeStatuses.has(status) ? periodEndMs : undefined,
        });
      }
      return { ok: true, duplicate: false };
    }

    if (type === "customer.subscription.deleted") {
      const sub = obj as StripeSubLike;
      let user: Doc<"users"> | null = null;
      if (sub.id) {
        user = await ctx.db
          .query("users")
          .withIndex("by_stripeSubscriptionId", (q) =>
            q.eq("stripeSubscriptionId", sub.id),
          )
          .unique();
      }
      if (!user && sub.customer) {
        user = await ctx.db
          .query("users")
          .withIndex("by_stripeCustomerId", (q) =>
            q.eq("stripeCustomerId", sub.customer),
          )
          .unique();
      }
      if (user) {
        await ctx.db.patch("users", user._id, {
          plusExpiresAt: undefined,
        });
      }
      return { ok: true, duplicate: false };
    }

    // invoice.paid / other — ignore (subscription.updated covers renewals).
    return { ok: true, duplicate: false, ignored: true };
  },
});
