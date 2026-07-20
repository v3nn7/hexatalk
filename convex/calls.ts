import { v } from "convex/values";
import { mutation, query, MutationCtx, QueryCtx } from "./_generated/server";
import { currentUser, isBlockedEitherWay } from "./session";
import { Doc, Id } from "./_generated/dataModel";

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

async function purgeIceCandidates(ctx: MutationCtx, callId: Id<"calls">) {
  const rows = await ctx.db
    .query("callIceCandidates")
    .withIndex("by_call", (q) => q.eq("callId", callId))
    .take(500);
  for (const row of rows) {
    await ctx.db.delete("callIceCandidates", row._id);
  }
}

async function logCallEvent(
  ctx: MutationCtx,
  conversationId: Id<"conversations">,
  authorId: Id<"users">,
  body: string,
) {
  const now = Date.now();
  await ctx.db.insert("messages", {
    conversationId,
    authorId,
    authorName: "",
    body,
    kind: "call",
  });
  await ctx.db.patch("conversations", conversationId, { lastMessageAt: now });
}

function formatDuration(ms: number): string {
  const totalSeconds = Math.max(0, Math.round(ms / 1000));
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes}:${seconds.toString().padStart(2, "0")}`;
}

async function findActiveCall(
  ctx: QueryCtx,
  userId: Id<"users">,
): Promise<Doc<"calls"> | null> {
  for (const status of ["active", "ringing"] as const) {
    const asCaller = await ctx.db
      .query("calls")
      .withIndex("by_caller_and_status", (q) =>
        q.eq("callerId", userId).eq("status", status),
      )
      .take(1);
    if (asCaller.length > 0) return asCaller[0];

    const asCallee = await ctx.db
      .query("calls")
      .withIndex("by_callee_and_status", (q) =>
        q.eq("calleeId", userId).eq("status", status),
      )
      .take(1);
    if (asCallee.length > 0) return asCallee[0];
  }
  return null;
}

/** Ringing calls unanswered for this long are considered stale (crashed
 * client, lost tab) and auto-ended so neither party is blocked forever. */
const RING_TIMEOUT_MS = 2 * 60 * 1000;
const SDP_MAX_LEN = 200_000;
const ICE_CANDIDATE_MAX_LEN = 16_000;

async function expireStaleRingingCalls(
  ctx: MutationCtx,
  userId: Id<"users">,
): Promise<void> {
  const cutoff = Date.now() - RING_TIMEOUT_MS;
  const [asCaller, asCallee] = await Promise.all([
    ctx.db
      .query("calls")
      .withIndex("by_caller_and_status", (q) =>
        q.eq("callerId", userId).eq("status", "ringing"),
      )
      .take(10),
    ctx.db
      .query("calls")
      .withIndex("by_callee_and_status", (q) =>
        q.eq("calleeId", userId).eq("status", "ringing"),
      )
      .take(10),
  ]);
  for (const call of [...asCaller, ...asCallee]) {
    if (call.startedAt >= cutoff) continue;
    await ctx.db.patch("calls", call._id, {
      status: "ended",
      endedAt: Date.now(),
    });
    await purgeIceCandidates(ctx, call._id);
    await logCallEvent(ctx, call.conversationId, call.callerId, "Missed call");
  }
}

export const startCall = mutation({
  args: {
    sessionToken: v.string(),
    conversationId: v.id("conversations"),
    calleeId: v.id("users"),
    offerSdp: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);

    if (args.calleeId === me._id) {
      throw new Error("You can't call yourself");
    }
    if (args.offerSdp.length < 10 || args.offerSdp.length > SDP_MAX_LEN) {
      throw new Error("Invalid offer");
    }

    // Clear zombie ringing calls first so a crashed client can't block
    // either party from ever calling again.
    await expireStaleRingingCalls(ctx, me._id);
    await expireStaleRingingCalls(ctx, args.calleeId);

    const existing = await findActiveCall(ctx, me._id);
    if (existing) {
      throw new Error("You're already in a call");
    }
    const calleeExisting = await findActiveCall(ctx, args.calleeId);
    if (calleeExisting) {
      throw new Error("This user is already in a call");
    }

    if (await isBlockedEitherWay(ctx, me._id, args.calleeId)) {
      throw new Error("You can't call this user");
    }

    await requireMembership(ctx, args.conversationId, me._id);

    const membership = await ctx.db
      .query("conversationMembers")
      .withIndex("by_conversation_and_user", (q) =>
        q.eq("conversationId", args.conversationId).eq("userId", args.calleeId),
      )
      .unique();
    if (!membership) {
      throw new Error("This user is not part of the chat");
    }

    const callId = await ctx.db.insert("calls", {
      conversationId: args.conversationId,
      callerId: me._id,
      calleeId: args.calleeId,
      status: "ringing",
      offerSdp: args.offerSdp,
      startedAt: Date.now(),
    });
    return callId;
  },
});

export const respond = mutation({
  args: {
    sessionToken: v.string(),
    callId: v.id("calls"),
    accept: v.boolean(),
    answerSdp: v.optional(v.string()),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const call = await ctx.db.get("calls", args.callId);
    if (!call || call.calleeId !== me._id) {
      throw new Error("Call not found");
    }
    if (call.status !== "ringing") {
      return null;
    }

    if (args.accept) {
      if (!args.answerSdp) {
        throw new Error("Missing answer");
      }
      if (args.answerSdp.length < 10 || args.answerSdp.length > SDP_MAX_LEN) {
        throw new Error("Invalid answer");
      }
      await ctx.db.patch("calls", call._id, {
        status: "active",
        answerSdp: args.answerSdp,
      });
    } else {
      await ctx.db.patch("calls", call._id, {
        status: "declined",
        endedAt: Date.now(),
      });
      await purgeIceCandidates(ctx, call._id);
      await logCallEvent(ctx, call.conversationId, me._id, "Call declined");
    }
    return null;
  },
});

export const endCall = mutation({
  args: {
    sessionToken: v.string(),
    callId: v.id("calls"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const call = await ctx.db.get("calls", args.callId);
    if (!call) return null;
    if (call.callerId !== me._id && call.calleeId !== me._id) {
      throw new Error("Not a participant in this call");
    }
    if (call.status === "ended" || call.status === "declined") {
      return null;
    }

    const wasActive = call.status === "active";
    const now = Date.now();
    await ctx.db.patch("calls", call._id, { status: "ended", endedAt: now });
    await purgeIceCandidates(ctx, call._id);

    const body = wasActive
      ? `Voice call ended · ${formatDuration(now - call.startedAt)}`
      : "Call cancelled";
    await logCallEvent(ctx, call.conversationId, me._id, body);
    return null;
  },
});

export const addIceCandidate = mutation({
  args: {
    sessionToken: v.string(),
    callId: v.id("calls"),
    candidate: v.string(),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const call = await ctx.db.get("calls", args.callId);
    if (!call || (call.callerId !== me._id && call.calleeId !== me._id)) {
      throw new Error("Call not found");
    }
    // No ICE for finished calls — otherwise ended calls collect junk rows.
    if (call.status === "ended" || call.status === "declined") {
      return null;
    }
    if (args.candidate.length === 0 || args.candidate.length > ICE_CANDIDATE_MAX_LEN) {
      throw new Error("Invalid ICE candidate");
    }
    await ctx.db.insert("callIceCandidates", {
      callId: args.callId,
      fromUserId: me._id,
      candidate: args.candidate,
    });
    return null;
  },
});

export const listPeerIceCandidates = query({
  args: {
    sessionToken: v.string(),
    callId: v.id("calls"),
  },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const call = await ctx.db.get("calls", args.callId);
    if (!call || (call.callerId !== me._id && call.calleeId !== me._id)) {
      return [];
    }
    const rows = await ctx.db
      .query("callIceCandidates")
      .withIndex("by_call", (q) => q.eq("callId", args.callId))
      .take(200);
    return rows
      .filter((row) => row.fromUserId !== me._id)
      .map((row) => ({ id: row._id, candidate: row.candidate }));
  },
});

export const myCall = query({
  args: { sessionToken: v.string() },
  handler: async (ctx, args) => {
    const me = await currentUser(ctx, args.sessionToken);
    const call = await findActiveCall(ctx, me._id);
    if (!call) return null;

    const isCaller = call.callerId === me._id;
    const peerId = isCaller ? call.calleeId : call.callerId;
    const peer = await ctx.db.get("users", peerId);

    return {
      callId: call._id,
      conversationId: call.conversationId,
      status: call.status,
      isCaller,
      peerUserId: peerId,
      peerDisplayName: peer?.displayName ?? "Unknown",
      offerSdp: call.offerSdp,
      answerSdp: call.answerSdp ?? null,
    };
  },
});
