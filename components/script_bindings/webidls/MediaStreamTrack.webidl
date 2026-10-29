/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/mediacapture-main/#dom-mediastreamtrack

[Exposed=Window]
interface MediaStreamTrack : EventTarget {
    readonly        attribute DOMString kind;
    readonly        attribute DOMString id;
    // readonly        attribute DOMString label;
    [SetterThrows] attribute boolean enabled;
    // readonly        attribute boolean muted;
    //                 attribute EventHandler onmute;
    //                 attribute EventHandler onunmute;
    readonly        attribute MediaStreamTrackState readyState;
    //                 attribute EventHandler onended;
    [Throws] MediaStreamTrack clone();
    [Throws] undefined stop();
    // MediaTrackCapabilities getCapabilities();
    MediaTrackConstraints getConstraints();
    MediaTrackSettings getSettings();
    // Promise<void> applyConstraints(optional MediaTrackConstraints constraints);
};

dictionary MediaTrackSettings {
    unsigned long width;
    unsigned long height;
    double aspectRatio;
    double frameRate;
    unsigned long sampleRate;
};

enum MediaStreamTrackState {
    "live",
    "ended"
};
