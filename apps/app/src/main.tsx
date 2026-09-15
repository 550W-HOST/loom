import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "./app.css";
import { App } from "./App.js";

const root = document.getElementById("root");

if (!root) {
  throw new Error("Product app root is missing");
}

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
