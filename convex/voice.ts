import { v } from "convex/values";
import { mutation, query, MutationCtx, QueryCtx } from "./_generated/server";
import { currentUser } from "./session";
import { Id } from "./_generated/dataModel";

async function requireMembership(
  ctx: QueryCtx | MutationCtx,
  conversationId: Id<"conversations">,
  userId: Id<"users">,
) {
  const membership = await ctx.db
    .query("conversationMembers")
    .withIndex("by_conversation_and_user", (q) =>
      q.eq("conversationId", conversationId).eq("userId", userId),
    )
    .unique();
  if (!membership) {
    throw new Error("You're not a member of this chat");
  }
}

/** Server voice channels + group voice rooms. */
function isVoiceRoom(
  kind: string,
  channelType: string | undefined,
): boolean {
  if (kind === "group") return true;
  if (kind === "channel" && (channelType ?? "text") === "voice") return true;
  return false;
}

function pairKey(a: Id<"users">, b: Id<"users">): string {
  return a < b ? `${a}|${b}` : `${b}|${a}`;
}

function offererOf(a: Id<"users">, b: Id<"users">): Id<"users"> {
  return a < b ? a : b;
}

function answererOf(a: Id<"users">, b: Id<"users">): Id<"users"> {
  return a < b ? b : a;
}

async function purgeLinkIce(ctx: MutationCtx, linkId: Id<"voiceLinks">) {
  const rows = await ctx.db
    .query("voiceLinkIce")
    .withIndex("by_link", (q) => q.eq("linkId", linkId))
    .take(500);
  for (const row of rows) {
    await ctx.db.delete("voiceLinkIce", row._id);
  }
}

async function endLinksForUser(
  ctx: MutationCtx,
  userId: Id<"users">,
  conversationId?: Id<"conversations">,
) {
  const asOfferer = await ctx.db
    .query("voiceLinks")
    .withIndex("by_offerer", (q) => q.eq("offererId", userId))
    .take(100);
  const asAnswerer = await ctx.db
    .query("voiceLinks")
    .withIndex("by_answerer", (q) => q.eq("answererId", userId))
    .take(100);
  for (const row of [...asOfferer, ...asAnswerer]) {
    if (conversationId && row.conversationId !== conversationId) continue;
    if (row.status === "ended") continue;
    await ctx.db.patch("voiceLinks", row._id, { status: "ended" });
    await purgeLinkIce(ctx, row._id);
  }
}

export const join = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const channel = await ctx.db.get("conversations", args.conversationId);
    if (!channel) {
      throw new Error("Conversation not found");
    }
    if (!isVoiceRoom(channel.kind, channel.channelType)) {
      throw new Error("Not a voice room (use a voice channel or group)");
    }
    await requireMembership(ctx, args.conversationId, me._id);

    // Leave any other voice room first (one room at a time).
    const mine = await ctx.db
      .query("voiceStates")
      .withIndex("by_user", (q) => q.eq("userId", me._id))
      .take(20);
    for (const row of mine) {
      if (row.conversationId !== args.conversationId) {
        await endLinksForUser(ctx, me._id, row.conversationId);
        await ctx.db.delete("voiceStates", row._id);
      }
    }

    const existing = await ctx.db
      .query("voiceStates")
      .withIndex("by_conversation_and_user", (q) =>
        q.eq("conversationId", args.conversationId).eq("userId", me._id),
      )
      .unique();
    if (existing) {
      await ctx.db.patch("voiceStates", existing._id, {
        displayName: me.displayName,
        joinedAt: Date.now(),
      });
      return existing._id;
    }
    return await ctx.db.insert("voiceStates", {
      conversationId: args.conversationId,
      userId: me._id,
      displayName: me.displayName,
      joinedAt: Date.now(),
    });
  },
});

export const leave = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.optional(v.id("conversations")),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (args.conversationId) {
      const row = await ctx.db
        .query("voiceStates")
        .withIndex("by_conversation_and_user", (q) =>
          q
            .eq("conversationId", args.conversationId!)
            .eq("userId", me._id),
        )
        .unique();
      if (row) await ctx.db.delete("voiceStates", row._id);
      await endLinksForUser(ctx, me._id, args.conversationId);
    } else {
      const mine = await ctx.db
        .query("voiceStates")
        .withIndex("by_user", (q) => q.eq("userId", me._id))
        .take(20);
      for (const row of mine) {
        await endLinksForUser(ctx, me._id, row.conversationId);
        await ctx.db.delete("voiceStates", row._id);
      }
    }
    return null;
  },
});

export const listInChannel = query({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requireMembership(ctx, args.conversationId, me._id);
    const rows = await ctx.db
      .query("voiceStates")
      .withIndex("by_conversation", (q) =>
        q.eq("conversationId", args.conversationId),
      )
      .take(100);
    return rows
      .map((r) => ({
        userId: r.userId,
        displayName: r.displayName,
        joinedAt: r.joinedAt,
      }))
      .sort((a, b) => a.joinedAt - b.joinedAt);
  },
});

/** Publish or replace the SDP offer for my ordered pair with `peerId`. */
export const publishOffer = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
    peerId: v.id("users"),
    offerSdp: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    if (args.peerId === me._id) {
      throw new Error("Cannot voice-link to yourself");
    }
    await requireMembership(ctx, args.conversationId, me._id);
    await requireMembership(ctx, args.conversationId, args.peerId);

    const room = await ctx.db.get("conversations", args.conversationId);
    if (!room || !isVoiceRoom(room.kind, room.channelType)) {
      throw new Error("Not a voice room");
    }

    // Both sides must currently be in the room.
    const meIn = await ctx.db
      .query("voiceStates")
      .withIndex("by_conversation_and_user", (q) =>
        q.eq("conversationId", args.conversationId).eq("userId", me._id),
      )
      .unique();
    const peerIn = await ctx.db
      .query("voiceStates")
      .withIndex("by_conversation_and_user", (q) =>
        q.eq("conversationId", args.conversationId).eq("userId", args.peerId),
      )
      .unique();
    if (!meIn || !peerIn) {
      throw new Error("Both users must be in the voice room");
    }

    const offererId = offererOf(me._id, args.peerId);
    if (offererId !== me._id) {
      throw new Error("You are not the offerer for this pair");
    }
    const answererId = answererOf(me._id, args.peerId);
    const key = pairKey(me._id, args.peerId);

    if (args.offerSdp.length < 10 || args.offerSdp.length > 200_000) {
      throw new Error("Invalid offer");
    }

    const existing = await ctx.db
      .query("voiceLinks")
      .withIndex("by_conversation_and_pair", (q) =>
        q.eq("conversationId", args.conversationId).eq("pairKey", key),
      )
      .unique();

    if (existing) {
      await purgeLinkIce(ctx, existing._id);
      await ctx.db.patch("voiceLinks", existing._id, {
        offerSdp: args.offerSdp,
        answerSdp: undefined,
        status: "offering",
        startedAt: Date.now(),
      });
      return existing._id;
    }

    return await ctx.db.insert("voiceLinks", {
      conversationId: args.conversationId,
      offererId,
      answererId,
      pairKey: key,
      offerSdp: args.offerSdp,
      status: "offering",
      startedAt: Date.now(),
    });
  },
});

export const publishAnswer = mutation({
  args: {
    sessionToken: v.string(),
    linkId: v.id("voiceLinks"),
    answerSdp: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const link = await ctx.db.get("voiceLinks", args.linkId);
    if (!link || link.answererId !== me._id) {
      throw new Error("Voice link not found");
    }
    if (link.status === "ended") {
      throw new Error("Voice link already ended");
    }
    if (args.answerSdp.length < 10 || args.answerSdp.length > 200_000) {
      throw new Error("Invalid answer");
    }
    await ctx.db.patch("voiceLinks", link._id, {
      answerSdp: args.answerSdp,
      status: "active",
    });
    return null;
  },
});

export const endLink = mutation({
  args: {
    sessionToken: v.string(),
    linkId: v.id("voiceLinks"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const link = await ctx.db.get("voiceLinks", args.linkId);
    if (!link) return null;
    if (link.offererId !== me._id && link.answererId !== me._id) {
      throw new Error("Not a participant");
    }
    if (link.status !== "ended") {
      await ctx.db.patch("voiceLinks", link._id, { status: "ended" });
      await purgeLinkIce(ctx, link._id);
    }
    return null;
  },
});

export const addLinkIce = mutation({
  args: {
    sessionToken: v.string(),
    linkId: v.id("voiceLinks"),
    candidate: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const link = await ctx.db.get("voiceLinks", args.linkId);
    if (!link || (link.offererId !== me._id && link.answererId !== me._id)) {
      throw new Error("Voice link not found");
    }
    if (link.status === "ended") return null;
    if (args.candidate.length > 16_000) {
      throw new Error("ICE candidate too large");
    }
    await ctx.db.insert("voiceLinkIce", {
      linkId: args.linkId,
      fromUserId: me._id,
      candidate: args.candidate,
    });
    return null;
  },
});

/** All non-ended links in this room that involve me (reactive mesh signaling). */
export const listMyLinks = query({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    await requireMembership(ctx, args.conversationId, me._id);

    const rows = await ctx.db
      .query("voiceLinks")
      .withIndex("by_conversation", (q) =>
        q.eq("conversationId", args.conversationId),
      )
      .take(200);

    return rows
      .filter(
        (r) =>
          r.status !== "ended" &&
          (r.offererId === me._id || r.answererId === me._id),
      )
      .map((r) => ({
        linkId: r._id,
        conversationId: r.conversationId,
        offererId: r.offererId,
        answererId: r.answererId,
        peerId: r.offererId === me._id ? r.answererId : r.offererId,
        isOfferer: r.offererId === me._id,
        offerSdp: r.offerSdp,
        answerSdp: r.answerSdp ?? null,
        status: r.status,
      }));
  },
});

export const listLinkIce = query({
  args: {
    sessionToken: v.string(),
    linkId: v.id("voiceLinks"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const link = await ctx.db.get("voiceLinks", args.linkId);
    if (!link || (link.offererId !== me._id && link.answererId !== me._id)) {
      return [];
    }
    const rows = await ctx.db
      .query("voiceLinkIce")
      .withIndex("by_link", (q) => q.eq("linkId", args.linkId))
      .take(300);
    return rows
      .filter((row) => row.fromUserId !== me._id)
      .map((row) => ({ id: row._id, candidate: row.candidate }));
  },
});
