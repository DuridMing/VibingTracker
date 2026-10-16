// WS client（contracts.md §6）。不碰 DOM；WebSocket 與計時器由外部注入，方便測試與 mock。

export const PROTOCOL = 1;

const BACKOFF_START_MS = 500;
const BACKOFF_MAX_MS = 10_000;
const MORE_LIMIT_MAX = 500;
const DISPATCH = ["welcome", "snapshot", "frame", "more"];

export function createClient({ url, WebSocket, setTimeout, clearTimeout, handlers = {}, utcOffsetMin = null }) {
  const channels = new Set();
  let ws = null;
  let lastFrame = null;
  let backoff = BACKOFF_START_MS;
  let timer = null;
  let stopped = false;

  const status = (s) => handlers.onStatus?.(s);
  const isOpen = () => ws !== null && ws.readyState === 1;
  const send = (msg) => {
    if (isOpen()) ws.send(JSON.stringify(msg));
  };

  function connect() {
    stopped = false;
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
    status("connecting");
    const sock = new WebSocket(url);
    ws = sock;
    sock.onopen = () => {
      const hello = { op: "hello", protocol: PROTOCOL };
      if (lastFrame !== null) hello.resume = lastFrame;
      if (typeof utcOffsetMin === "number") hello.utcOffsetMin = utcOffsetMin;
      send(hello);
      for (const channel of channels) send({ op: "subscribe", channel });
    };
    sock.onmessage = (ev) => onMessage(ev.data);
    // 瀏覽器在 error 之後一定補 close；Node（undici）連不上時只發 error、readyState 停在 0，
    // 所以兩個都當成斷線，只處理第一個（ws !== sock 之後的都忽略）
    const lost = () => {
      if (ws !== sock) return;
      ws = null;
      status("closed");
      if (stopped) return;
      timer = setTimeout(() => {
        timer = null;
        connect();
      }, backoff);
      backoff = Math.min(backoff * 2, BACKOFF_MAX_MS);
    };
    sock.onclose = lost;
    sock.onerror = lost;
  }

  function onMessage(data) {
    let msg;
    try {
      msg = JSON.parse(data);
    } catch (e) {
      handlers.onError?.(e);
      return;
    }
    if (msg === null || typeof msg !== "object") return;
    if (msg.t === "welcome") {
      if (msg.protocol !== PROTOCOL) {
        handlers.onIncompatible?.(msg);
        close();
        return;
      }
      backoff = BACKOFF_START_MS;
      status("open");
    }
    if (typeof msg.frame === "number") lastFrame = msg.frame;
    if (DISPATCH.includes(msg.t)) handlers[msg.t]?.(msg);
  }

  function close() {
    stopped = true;
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
    ws?.close();
  }

  return {
    connect,
    close,
    subscribe(channel) {
      if (channels.has(channel)) return;
      channels.add(channel);
      send({ op: "subscribe", channel });
    },
    unsubscribe(channel) {
      if (!channels.delete(channel)) return;
      send({ op: "unsubscribe", channel });
    },
    ack({ session, kind, ref }) {
      send({ op: "ack", session, kind, ref });
    },
    more({ session, beforePos, limit }) {
      send({ op: "more", session, beforePos, limit: Math.min(limit, MORE_LIMIT_MAX) });
    },
  };
}
