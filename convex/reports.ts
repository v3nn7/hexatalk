import { v } from "convex/values";
import { mutation, query, MutationCtx, QueryCtx } from "./_generated/server";
import { currentUser, requireStaff } from "./session";
import { Id } from "./_generated/dataModel";

const SNAPSHOT_MAX_LEN = 2000;

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

/**
 * Flags a message for staff review. `messageBody` is supplied by the
 * client (not read from `messages.body` server-side) because DM bodies may
 * be E2EE ciphertext the server never decrypts — the client already has
 * the plaintext it's rendering on screen.
 */
export const reportMessage = mutation({
  args: {
    sessionToken: v.string(),
    messageId: v.id("messages"),
    messageBody: v.string(),
    reason: v.union(
      v.literal("spam"),
      v.literal("harassment"),
      v.literal("illegal_content"),
      v.literal("other"),
    ),
  },
  handler: async (ctx, args) => {
    const reporter = await currentUser(ctx, args.sessionToken);
    const message = await ctx.db.get("messages", args.messageId);
    if (!message) {
      throw new Error("Message not found");
    }
    if (message.authorId === reporter._id) {
      throw new Error("You can't report your own message");
    }
    // Only people who can actually see the message may report it — stops
    // probing / report-spam against arbitrary message ids.
    await requireMembership(ctx, message.conversationId, reporter._id);

    const already = await ctx.db
      .query("messageReports")
      .withIndex("by_message_and_reporter", (q) =>
        q.eq("messageId", args.messageId).eq("reporterId", reporter._id),
      )
      .first();
    if (already) {
      throw new Error("You already reported this message");
    }

    const conversation = await ctx.db.get(
      "conversations",
      message.conversationId,
    );
    const conversationLabel =
      conversation?.name?.trim() ||
      (conversation?.kind === "direct" ? "Direct message" : "Conversation");

    await ctx.db.insert("messageReports", {
      messageId: args.messageId,
      conversationId: message.conversationId,
      conversationLabel,
      reporterId: reporter._id,
      reporterUsername: reporter.username,
      authorId: message.authorId,
      authorUsername: message.authorName,
      messageBodySnapshot: args.messageBody.slice(0, SNAPSHOT_MAX_LEN),
      reason: args.reason,
      status: "pending",
      createdAt: Date.now(),
    });
    return null;
  },
});

/** Staff-only: pending/actioned/dismissed report queue for the admin panel. */
export const adminListReports = query({
  args: {
    sessionToken: v.string(),
    status: v.optional(
      v.union(
        v.literal("pending"),
        v.literal("actioned"),
        v.literal("dismissed"),
      ),
    ),
  },
  handler: async (ctx, args) => {
    await requireStaff(ctx, args.sessionToken);
    const status = args.status ?? "pending";
    const reports = await ctx.db
      .query("messageReports")
      .withIndex("by_status", (q) => q.eq("status", status))
      .order("desc")
      .take(200);
    return reports.map((r) => ({
      reportId: r._id,
      messageId: r.messageId,
      conversationLabel: r.conversationLabel,
      reporterUsername: r.reporterUsername,
      authorUsername: r.authorUsername,
      messageBody: r.messageBodySnapshot,
      reason: r.reason,
      status: r.status,
      createdAt: r.createdAt,
      reviewedByUsername: r.reviewedByUsername ?? "",
      reviewNote: r.reviewNote ?? "",
    }));
  },
});

/** Staff-only: mark a report actioned (punishment was applied elsewhere in
 * the admin panel) or dismissed (deemed baseless). Log-only system — this
 * never bans or deletes anything by itself. */
export const adminResolveReport = mutation({
  args: {
    sessionToken: v.string(),
    reportId: v.id("messageReports"),
    status: v.union(v.literal("actioned"), v.literal("dismissed")),
    reviewNote: v.optional(v.string()),
  },
  handler: async (ctx, args) => {
    const staff = await requireStaff(ctx, args.sessionToken);
    const report = await ctx.db.get("messageReports", args.reportId);
    if (!report) {
      throw new Error("Report not found");
    }
    await ctx.db.patch("messageReports", args.reportId, {
      status: args.status,
      reviewedBy: staff._id,
      reviewedByUsername: staff.username,
      reviewedAt: Date.now(),
      reviewNote: args.reviewNote?.trim() || undefined,
    });
    return null;
  },
});
