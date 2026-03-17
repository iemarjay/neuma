import React from "react";
import ReactDOM from "react-dom/client";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import App from "./App";
import Settings from "./Settings";
import Startup from "./Startup";
import "./index.css";

const label = getCurrentWebviewWindow().label;

function Root() {
  if (label === "settings") return <Settings />;
  if (label === "startup") return <Startup />;
  return <App />;
}

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <Root />
  </React.StrictMode>
);
