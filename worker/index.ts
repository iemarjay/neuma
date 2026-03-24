/// <reference types="@cloudflare/workers-types" />

export interface Env {
  AI: Ai;
  API_KEY: string;
  BREVO_API_KEY: string;
  BREVO_LIST_ID: string; // numeric list ID from Brevo dashboard, stored as string
}

interface CleanupRequest {
  text: string;
  context?: string;
}

interface CleanupResponse {
  result: string;
}

const CORS_HEADERS: Record<string, string> = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "POST, OPTIONS",
  "Access-Control-Allow-Headers": "Content-Type, X-API-Key",
};

function corsResponse(body: string, status = 200, contentType = "application/json"): Response {
  return new Response(body, {
    status,
    headers: {
      ...CORS_HEADERS,
      "Content-Type": contentType,
    },
  });
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    // Handle CORS preflight
    if (request.method === "OPTIONS") {
      return new Response(null, { status: 204, headers: CORS_HEADERS });
    }

    const url = new URL(request.url);

    // API key validation for all non-preflight requests
    const apiKey = request.headers.get("X-API-Key");
    if (!apiKey || apiKey !== env.API_KEY) {
      return corsResponse(JSON.stringify({ error: "unauthorized" }), 401);
    }

    if (url.pathname === "/ping") {
      return corsResponse(JSON.stringify({ ok: true }));
    }

    // Public endpoint — no API key required
    if (url.pathname === "/subscribe") {
      return handleSubscribe(request, env);
    }

    if (url.pathname !== "/cleanup") {
      return corsResponse(JSON.stringify({ error: "not found" }), 404);
    }

    if (request.method !== "POST") {
      return corsResponse(JSON.stringify({ error: "method not allowed" }), 405);
    }

    let body: CleanupRequest;
    try {
      body = (await request.json()) as CleanupRequest;
    } catch {
      return corsResponse(JSON.stringify({ error: "invalid JSON body" }), 400);
    }

    if (!body.text || typeof body.text !== "string") {
      return corsResponse(JSON.stringify({ error: "missing or invalid 'text' field" }), 400);
    }

    const inputText = body.text.trim();
    if (inputText.length === 0) {
      return corsResponse(JSON.stringify({ result: "" }));
    }

    const contextSection = body.context?.trim()
      ? `\n- If any names or terms in the following document context match phonetically with words in the transcript, use their exact spelling:\nDocument context (text before cursor):\n${body.context.trim()}\n`
      : "";

    const prompt = `You are a voice dictation cleanup engine. Your job is to transform raw speech transcription into clean, natural written text.

Rules:
- Remove filler words (um, uh, like, you know, basically, literally)
- Remove false starts and self-corrections — keep only the intended version (e.g. "let's meet at 2... actually 3" → "let's meet at 3")
- Fix punctuation and capitalization naturally
- Preserve the speaker's tone and vocabulary — do not rewrite or rephrase
- Convert spoken list cues ("one... two..." or "first... second...") into a newline-separated list using "- " bullets
- Convert "new line" or "new paragraph" into actual line breaks
- Convert spoken punctuation ("exclamation point", "question mark", "comma", "period") into the actual symbol
- Do not add, infer, or expand on anything not spoken${contextSection}
- Output only the cleaned text, nothing else

Text: ${inputText}`;

    let result: string;
    try {
      const aiResponse = await env.AI.run("@cf/meta/llama-3.2-1b-instruct", {
        messages: [
          {
            role: "user",
            content: prompt,
          },
        ],
        max_tokens: 1024,
      });

      // Workers AI returns { response: string } for text generation models
      const response = aiResponse as { response?: string };
      result = (response.response ?? "").trim();

      // Strip common preamble the model adds despite instructions (e.g. "Here is the cleaned text:\n\n")
      result = result.replace(/^(here is (the )?cleaned (text|version)[:\s]*\n*)/i, "").trim();
      // Strip surrounding quotes the model sometimes adds
      result = result.replace(/^"([\s\S]*)"$/, "$1").trim();

      // If AI returned nothing useful, fall back to the original text
      if (!result) {
        result = inputText;
      }
    } catch (err) {
      console.error("Workers AI error:", err);
      return corsResponse(
        JSON.stringify({ error: "AI inference failed", details: String(err) }),
        502
      );
    }

    const responseBody: CleanupResponse = { result };
    return corsResponse(JSON.stringify(responseBody));
  },
};

async function handleSubscribe(request: Request, env: Env): Promise<Response> {
  if (request.method === "OPTIONS") {
    return new Response(null, { status: 204, headers: CORS_HEADERS });
  }
  if (request.method !== "POST") {
    return corsResponse(JSON.stringify({ error: "method not allowed" }), 405);
  }

  let body: { email?: string };
  try {
    body = (await request.json()) as { email?: string };
  } catch {
    return corsResponse(JSON.stringify({ error: "invalid JSON body" }), 400);
  }

  const email = (body.email ?? "").trim().toLowerCase();
  if (!email || !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email)) {
    return corsResponse(JSON.stringify({ error: "invalid email" }), 400);
  }

  if (!env.BREVO_API_KEY) {
    console.error("BREVO_API_KEY not set");
    return corsResponse(JSON.stringify({ error: "server misconfiguration" }), 500);
  }

  const listId = parseInt(env.BREVO_LIST_ID ?? "0", 10);
  const payload: Record<string, unknown> = { email, updateEnabled: true };
  if (listId > 0) payload.listIds = [listId];

  const brevoRes = await fetch("https://api.brevo.com/v3/contacts", {
    method: "POST",
    headers: {
      "api-key": env.BREVO_API_KEY,
      "Content-Type": "application/json",
      Accept: "application/json",
    },
    body: JSON.stringify(payload),
  });

  // 201 = created, 204 = already exists and updated
  if (brevoRes.status === 201 || brevoRes.status === 204) {
    return corsResponse(JSON.stringify({ ok: true }));
  }

  const errBody = await brevoRes.text();
  console.error("Brevo error", brevoRes.status, errBody);
  return corsResponse(JSON.stringify({ error: "failed to subscribe" }), 502);
}
