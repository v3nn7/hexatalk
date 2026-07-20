/**
 * HTTP routes for Convex.
 * Stripe webhook: POST /stripe/webhook
 *
 * Production endpoint (live):
 *   https://REDACTED.convex.example.com/stripe/webhook
 *
 * Setup:
 *  1. Deploy Convex (npx convex deploy)
 *  2. Stripe Dashboard → Developers → Webhooks → Add endpoint
 *     URL: https://REDACTED.convex.example.com/stripe/webhook
 *     Events: checkout.session.completed,
 *             customer.subscription.created,
 *             customer.subscription.updated,
 *             customer.subscription.deleted
 *  3. Copy signing secret (whsec_…) → Convex env STRIPE_WEBHOOK_SECRET
 *     (Dashboard → Settings → Environment Variables, production deployment)
 *  4. Also set STRIPE_SECRET_KEY + STRIPE_PRICE_ID for Checkout
 */
import { httpRouter } from "convex/server";
import { httpAction } from "./_generated/server";
import { internal } from "./_generated/api";

// No @types/node in this project; Convex runtime provides process.env.
declare const process: { env: Record<string, string | undefined> };

const http = httpRouter();

/** Stripe signature: t=timestamp,v1=hex_hmac_sha256 */
async function verifyStripeSignature(
  rawBody: string,
  signatureHeader: string | null,
  secret: string,
): Promise<boolean> {
  if (!signatureHeader) return false;
  const parts = signatureHeader.split(",").map((p) => p.trim());
  let timestamp = "";
  const v1s: string[] = [];
  for (const part of parts) {
    const [k, v] = part.split("=");
    if (k === "t") timestamp = v ?? "";
    if (k === "v1" && v) v1s.push(v);
  }
  if (!timestamp || v1s.length === 0) return false;

  // Reject if older than 5 minutes (replay protection).
  const tsNum = Number(timestamp);
  if (!Number.isFinite(tsNum)) return false;
  const ageSec = Math.abs(Date.now() / 1000 - tsNum);
  if (ageSec > 300) return false;

  const signedPayload = `${timestamp}.${rawBody}`;
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const mac = await crypto.subtle.sign(
    "HMAC",
    key,
    new TextEncoder().encode(signedPayload),
  );
  const expected = Array.from(new Uint8Array(mac))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");

  // Constant-time-ish compare of hex strings.
  for (const v1 of v1s) {
    if (v1.length !== expected.length) continue;
    let ok = 0;
    for (let i = 0; i < expected.length; i++) {
      ok |= expected.charCodeAt(i) ^ v1.charCodeAt(i);
    }
    if (ok === 0) return true;
  }
  return false;
}

http.route({
  path: "/stripe/webhook",
  method: "POST",
  handler: httpAction(async (ctx, req) => {
    const secret = process.env.STRIPE_WEBHOOK_SECRET;
    if (!secret) {
      console.error("STRIPE_WEBHOOK_SECRET is not set");
      return new Response("Webhook not configured", { status: 500 });
    }

    const rawBody = await req.text();
    const sig = req.headers.get("stripe-signature");
    const valid = await verifyStripeSignature(rawBody, sig, secret);
    if (!valid) {
      return new Response("Invalid signature", { status: 400 });
    }

    let event: {
      id?: string;
      type?: string;
      data?: { object?: unknown };
    };
    try {
      event = JSON.parse(rawBody) as typeof event;
    } catch {
      return new Response("Invalid JSON", { status: 400 });
    }

    if (!event.id || !event.type || !event.data?.object) {
      return new Response("Malformed event", { status: 400 });
    }

    try {
      await ctx.runMutation(internal.plus.processStripeEvent, {
        eventId: event.id,
        type: event.type,
        objectJson: JSON.stringify(event.data.object),
      });
    } catch (err) {
      console.error("Stripe webhook handler error", err);
      return new Response("Handler error", { status: 500 });
    }

    return new Response(JSON.stringify({ received: true }), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    });
  }),
});

export default http;
