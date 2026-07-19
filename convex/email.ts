import { v } from "convex/values";
import {
  action,
  internalAction,
  internalMutation,
  internalQuery,
  mutation,
} from "./_generated/server";
import { internal } from "./_generated/api";
import { Doc } from "./_generated/dataModel";
import { currentUser } from "./session";
import { hashSessionToken, timingSafeEqual } from "./auth";

// No @types/node in this project; Convex's runtime provides `process.env`
// regardless, this just satisfies the standalone typecheck.
declare const process: { env: Record<string, string | undefined> };

const CODE_TTL_MS = 15 * 60 * 1000;
const MAX_ATTEMPTS = 5;
/** Min wait between verification-code emails for one user. */
const MIN_RESEND_INTERVAL_MS = 60 * 1000;

function generateCode(): string {
  const bytes = new Uint8Array(4);
  crypto.getRandomValues(bytes);
  const num = new DataView(bytes.buffer).getUint32(0) % 1_000_000;
  return num.toString().padStart(6, "0");
}

async function sendVerificationEmail(email: string, code: string): Promise<void> {
  const apiKey = process.env.RESEND_API_KEY;
  if (!apiKey) {
    throw new Error("Email sending is not configured (missing RESEND_API_KEY)");
  }
  const from = process.env.RESEND_FROM_EMAIL ?? "HexaTalk <onboarding@resend.dev>";

  const res = await fetch("https://api.resend.com/emails", {
    method: "POST",
    headers: {
      Authorization: `Bearer ${apiKey}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      from,
      to: [email],
      subject: "Your HexaTalk verification code",
      text: `Your HexaTalk verification code is ${code}. It expires in 15 minutes.`,
      html:
        `<p>Your HexaTalk verification code is ` +
        `<strong style="font-size:20px">${code}</strong>.</p>` +
        `<p>It expires in 15 minutes. If you didn't request this, ignore this email.</p>`,
    }),
  });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`Could not send verification email (${res.status}): ${body}`);
  }
}

/**
 * Generates + stores + emails a fresh code for (userId, email). Internal —
 * called from the public `requestEmailVerification` below and from
 * `auth:signUp` (which needs to kick off verification for the email
 * collected at signup, before a sessionToken-checking action makes sense).
 * Does NOT touch `users.email` yet — that only happens once the code is
 * confirmed, so `users.email` never holds an unproven address.
 */
export const issueCodeForUser = internalAction({
  args: { userId: v.id("users"), email: v.string() },
  handler: async (ctx, args) => {
    const code = generateCode();
    await ctx.runMutation(internal.email.storeVerificationCode, {
      userId: args.userId,
      email: args.email,
      codeHash: await hashSessionToken(code),
      expiresAt: Date.now() + CODE_TTL_MS,
    });
    await sendVerificationEmail(args.email, code);
  },
});

export const requestEmailVerification = action({
  args: { sessionToken: v.string(), email: v.string() },
  handler: async (ctx, args) => {
    const email = args.email.trim().toLowerCase();
    if (email.length > 254 || !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email)) {
      throw new Error("Enter a valid email address");
    }

    const user: Doc<"users"> = await ctx.runQuery(internal.auth.resolveSessionUser, {
      sessionToken: args.sessionToken,
    });

    // Cheap throttle: one code email per minute per user (each send is a
    // real email through the provider — unbounded resends are an abuse /
    // cost vector). Sent-at is derived from expiresAt - CODE_TTL_MS.
    const codeState = await ctx.runQuery(internal.email.getVerificationCodeState, {
      userId: user._id,
    });
    const sinceLast = Date.now() - codeState.lastSentAt;
    if (codeState.lastSentAt > 0 && sinceLast < MIN_RESEND_INTERVAL_MS) {
      const waitSeconds = Math.ceil((MIN_RESEND_INTERVAL_MS - sinceLast) / 1000);
      throw new Error(`Please wait ${waitSeconds}s before requesting another code`);
    }

    const existing = await ctx.runQuery(internal.email.getUserByEmail, { email });
    if (existing && existing._id !== user._id) {
      throw new Error("This email is already in use");
    }

    await ctx.runAction(internal.email.issueCodeForUser, { userId: user._id, email });
    return null;
  },
});

export const verifyEmailCode = mutation({
  args: { sessionToken: v.string(), code: v.string() },
  handler: async (ctx, args) => {
    const user = await currentUser(ctx, args.sessionToken);
    const row = await ctx.db
      .query("emailVerificationCodes")
      .withIndex("by_userId", (q) => q.eq("userId", user._id))
      .unique();
    if (!row) {
      throw new Error("No verification code pending — request a new one");
    }
    if (row.expiresAt < Date.now()) {
      await ctx.db.delete("emailVerificationCodes", row._id);
      throw new Error("Code expired — request a new one");
    }
    if (row.attempts >= MAX_ATTEMPTS) {
      await ctx.db.delete("emailVerificationCodes", row._id);
      throw new Error("Too many attempts — request a new code");
    }

    const attemptHash = await hashSessionToken(args.code.trim());
    if (!timingSafeEqual(attemptHash, row.codeHash)) {
      await ctx.db.patch("emailVerificationCodes", row._id, {
        attempts: row.attempts + 1,
      });
      throw new Error("Incorrect code");
    }

    // Make sure nobody else claimed this email while the code was pending.
    const existing = await ctx.db
      .query("users")
      .withIndex("by_email", (q) => q.eq("email", row.email))
      .unique();
    if (existing && existing._id !== user._id) {
      await ctx.db.delete("emailVerificationCodes", row._id);
      throw new Error("This email is already in use");
    }

    await ctx.db.patch("users", user._id, {
      email: row.email,
      emailVerified: true,
    });
    await ctx.db.delete("emailVerificationCodes", row._id);
    return null;
  },
});

export const getUserByEmail = internalQuery({
  args: { email: v.string() },
  handler: async (ctx, args) => {
    return await ctx.db
      .query("users")
      .withIndex("by_email", (q) => q.eq("email", args.email))
      .unique();
  },
});

/** When the current pending code was issued (0 = no code pending). */
export const getVerificationCodeState = internalQuery({
  args: { userId: v.id("users") },
  handler: async (ctx, args) => {
    const row = await ctx.db
      .query("emailVerificationCodes")
      .withIndex("by_userId", (q) => q.eq("userId", args.userId))
      .unique();
    return { lastSentAt: row ? row.expiresAt - CODE_TTL_MS : 0 };
  },
});

export const storeVerificationCode = internalMutation({
  args: {
    userId: v.id("users"),
    email: v.string(),
    codeHash: v.string(),
    expiresAt: v.number(),
  },
  handler: async (ctx, args) => {
    const existing = await ctx.db
      .query("emailVerificationCodes")
      .withIndex("by_userId", (q) => q.eq("userId", args.userId))
      .unique();
    if (existing) {
      await ctx.db.patch("emailVerificationCodes", existing._id, {
        email: args.email,
        codeHash: args.codeHash,
        expiresAt: args.expiresAt,
        attempts: 0,
      });
    } else {
      await ctx.db.insert("emailVerificationCodes", { ...args, attempts: 0 });
    }
  },
});
