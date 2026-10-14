"use strict";
// Loaded before the body so that the first paint already has the platform's
// panel shape: WebKitGTK does not clear pixels that later turn transparent.
if (navigator.userAgent.includes("Mac"))
  document.documentElement.classList.add("macos");
else if (navigator.userAgent.includes("Linux"))
  document.documentElement.classList.add("linux");
