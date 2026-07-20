import { v } from "convex/values";
import { mutation, query } from "./_generated/server";
import { currentUser } from "./session";

/** Invites may live at most this long — no effectively-permanent invites. */
const MAX_INVITE_TTL_MS = 7 * 24 * 60 * 60 * 1000;

/**
 * Host publishes a peerseal invite (`ps1:…`) for a direct conversation.
 * Replaces any previous invite for that chat.
 */
export const publishInvite = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
    invitePayload: v.string(),
    expiresAt: v.number(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const conversation = await ctx.db.get("conversations", args.conversationId);
    if (!conversation || conversation.kind !== "direct") {
      throw new Error("peerseal invites are only for direct chats");
    }

    const membership = await ctx.db
      .query("conversationMembers")
      .withIndex("by_conversation_and_user", (q) =>
        q.eq("conversationId", args.conversationId).eq("userId", me._id),
      )
      .unique();
    if (!membership) {
      throw new Error("You're not a member of this chat");
    }

    const payload = args.invitePayload.trim();
    if (payload.length < 16 || payload.length > 8000) {
      throw new Error("Invalid invite payload");
    }
    if (args.expiresAt < Date.now()) {
      throw new Error("Invite already expired");
    }
    if (args.expiresAt > Date.now() + MAX_INVITE_TTL_MS) {
      throw new Error("Invite expiry is too far in the future");
    }

    const existing = await ctx.db
      .query("peerInvites")
      .withIndex("by_conversation", (q) =>
        q.eq("conversationId", args.conversationId),
      )
      .unique();
    if (existing) {
      await ctx.db.patch("peerInvites", existing._id, {
        hostUserId: me._id,
        invitePayload: payload,
        expiresAt: args.expiresAt,
      });
    } else {
      await ctx.db.insert("peerInvites", {
        conversationId: args.conversationId,
        hostUserId: me._id,
        invitePayload: payload,
        expiresAt: args.expiresAt,
      });
    }
    return null;
  },
});

/** Drop the invite for a conversation (host or either member after hangup). */
export const clearInvite = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const membership = await ctx.db
      .query("conversationMembers")
      .withIndex("by_conversation_and_user", (q) =>
        q.eq("conversationId", args.conversationId).eq("userId", me._id),
      )
      .unique();
    if (!membership) {
      throw new Error("You're not a member of this chat");
    }
    const existing = await ctx.db
      .query("peerInvites")
      .withIndex("by_conversation", (q) =>
        q.eq("conversationId", args.conversationId),
      )
      .unique();
    if (existing) {
      await ctx.db.delete("peerInvites", existing._id);
    }
    return null;
  },
});

/**
 * Guest (or host) reads the current invite. Returns null if none / expired /
 * caller is the host (host already has the payload).
 */
export const getInvite = query({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const membership = await ctx.db
      .query("conversationMembers")
      .withIndex("by_conversation_and_user", (q) =>
        q.eq("conversationId", args.conversationId).eq("userId", me._id),
      )
      .unique();
    if (!membership) {
      throw new Error("You're not a member of this chat");
    }

    const row = await ctx.db
      .query("peerInvites")
      .withIndex("by_conversation", (q) =>
        q.eq("conversationId", args.conversationId),
      )
      .unique();
    if (!row) {
      return null;
    }
    if (row.expiresAt < Date.now()) {
      return null;
    }
    return {
      hostUserId: row.hostUserId,
      invitePayload: row.invitePayload,
      expiresAt: row.expiresAt,
      isHost: row.hostUserId === me._id,
    };
  },
});
