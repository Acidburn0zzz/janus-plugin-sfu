const params = new URLSearchParams(location.search.slice(1));
var USER_ID = Math.floor(Math.random() * (1000000001));
const roomId = params.get("room") || 42;
const mic = !/0|false|off/i.test(params.get("mic"));

Minijanus.verbose = true;

const PEER_CONNECTION_CONFIG = {
  iceServers: [
    { urls: "stun:stun.l.google.com:19302" },
    { urls: "stun:global.stun.twilio.com:3478?transport=udp" }
  ]
};

// global helper for interactive use
var c = {
  session: null,
  publisher: null,
  subscribers: {}
};

const status = document.getElementById("status");
function showStatus(message) {
  status.textContent = message;
}

function init() {
  const server = `wss://${location.hostname}:8989`;
  showStatus(`Connecting to ${server}...`);
  var ws = new WebSocket(server, "janus-protocol");
  ws.addEventListener("open", () => {
    var session = c.session = new Minijanus.JanusSession(ws.send.bind(ws));
    ws.addEventListener("message", ev => handleMessage(session, ev));
    session.create().then(() => attachPublisher(session)).then(x => {
      c.publisher = x;
    });
  });
}

function handleMessage(session, ev) {
  var data = JSON.parse(ev.data);
  session.receive(data);
  if (data.janus === "event") {
    if (data.plugindata && data.plugindata.data) {
      var contents = data.plugindata.data;
      switch (contents.event) {
      case "join":
        if (contents.room_id === roomId) {
          addUser(session, contents.user_id);
        }
        break;
      case "leave":
        if (contents.room_id === roomId) {
          removeUser(session, contents.user_id);
        }
        break;
      case undefined:
        // a non-plugin event
        break;
      default:
        console.error("Unknown event received: ", data.plugindata.data);
        break;
      }
    }
  }
}

function negotiateIce(conn, handle) {
  return new Promise((resolve, reject) => {
    conn.addEventListener("icecandidate", ev => {
      handle.sendTrickle(ev.candidate || null).then(() => {
        if (!ev.candidate) { // this was the last candidate on our end and now they received it
          resolve();
        }
      });
    });
  });
};

function addUser(session, userId) {
  console.info("Adding user " + userId + ".");
  attachSubscriber(session, userId).then(x => {
    c.subscribers[userId] = x;
  });
}

function removeUser(session, userId) {
  console.info("Removing user " + userId + ".");
  var subscriber = c.subscribers[userId];
  if (subscriber != null) {
    subscriber.handle.detach();
    subscriber.conn.close();
    delete c.subscribers[userId];
  }
}

let messages = [];

const messageCount = document.getElementById("messageCount");
function updateMessageCount() {
  messageCount.textContent = messages.length;
}

let firstMessageTime;
function storeMessage(data, reliable) {
  if (!firstMessageTime) {
    firstMessageTime = performance.now();
  }
  messages.push({
    time: performance.now() - firstMessageTime,
    reliable,
    message: JSON.parse(data)
  });
  updateMessageCount();
}

function storeReliableMessage(ev) {
  storeMessage(ev.data, true);
}

function storeUnreliableMessage(ev) {
  storeMessage(ev.data, false);
}

document.getElementById("saveButton").addEventListener("click", function saveToMessagesFile() {
  const file = new File([JSON.stringify(messages)], "messages.json", {type: "text/json"});
  saveAs(file);
});

document.getElementById("clearButton").addEventListener("click", function clearMessages() {
  messages = [];
  updateMessageCount();
});

function attachPublisher(session) {
  console.info("Attaching publisher for session: ", session);
  var conn = new RTCPeerConnection(PEER_CONNECTION_CONFIG);
  var handle = new Minijanus.JanusPluginHandle(session);
  return handle.attach("janus.plugin.sfu").then(() => {
    var iceReady = negotiateIce(conn, handle);

    var channel = conn.createDataChannel("reliable", { ordered: true });
    channel.addEventListener("message", storeReliableMessage);

    const unreliableChannel = conn.createDataChannel("unreliable", { ordered: false, maxRetransmits: 0 });
    unreliableChannel.addEventListener("message", storeUnreliableMessage);

    var mediaReady = mic ? navigator.mediaDevices.getUserMedia({ audio: true }) : Promise.reject();
    var offerReady = mediaReady
      .then(
        media => {
          conn.addStream(media);
          return conn.createOffer({ audio: true })
        },
        () => conn.createOffer()
      );
    var localReady = offerReady.then(conn.setLocalDescription.bind(conn));
    var remoteReady = offerReady
        .then(handle.sendJsep.bind(handle))
        .then(answer => conn.setRemoteDescription(answer.jsep));
    showStatus(`Connecting WebRTC...`);
    var connectionReady = Promise.all([iceReady, localReady, remoteReady]);
    return connectionReady
      .then(() => {
        showStatus(`Joining room ${roomId}...`);
        return handle.sendMessage({ kind: "join", room_id: roomId, user_id: USER_ID, notify: true });
      })
      .then(reply => {
        var response = reply.plugindata.data.response;
        showStatus(`Joined room ${roomId}`);
        response.user_ids.forEach(otherId => {
          addUser(session, otherId);
        });
        return { handle, conn, channel, unreliableChannel };
      });
  });
}

function attachSubscriber(session, otherId) {
  console.info("Attaching subscriber to " + otherId + " for session: ", session);
  var conn = new RTCPeerConnection(PEER_CONNECTION_CONFIG);
  conn.addEventListener("track", function(ev) {
    if (ev.track.kind === "audio") {
      var audioEl = document.createElement("audio");
      audioEl.controls = true;
      document.body.appendChild(audioEl);
      audioEl.srcObject = ev.streams[0];
      audioEl.play();
    } else if (ev.track.kind === "video") {
      var videoEl = document.createElement("video");
      videoEl.controls = true;
      document.body.appendChild(videoEl);
      videoEl.srcObject = ev.streams[0];
      videoEl.play();
    }
  });

  var handle = new Minijanus.JanusPluginHandle(session);
  return handle.attach("janus.plugin.sfu")
    .then(() => {
      var iceReady = negotiateIce(conn, handle);
      var offerReady = conn.createOffer({ offerToReceiveAudio: true, offerToReceiveVideo: true });
      var localReady = offerReady.then(conn.setLocalDescription.bind(conn));
      var remoteReady = offerReady
          .then(handle.sendJsep.bind(handle))
          .then(answer => conn.setRemoteDescription(answer.jsep));
      var connectionReady = Promise.all([iceReady, localReady, remoteReady]);
      return connectionReady.then(() => {
        return handle.sendMessage({ kind: "join", room_id: roomId, user_id: USER_ID, subscription_specs: [
          { content_kind: "all", publisher_id: otherId }
        ]});

      }).then(reply => {
        return { handle: handle, conn: conn };
      });

    });
}

init();
