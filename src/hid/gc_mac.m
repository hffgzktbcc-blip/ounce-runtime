// OunceCore's physical controllers on the Mac: Apple's GameController.framework (DualSense, DualShock 4, Xbox, Switch Pro,
// Joy-Con, MFi), read by HostGamepadManager (src/hid/gamepad_mac.rs).
//
// Face buttons are reported by POSITION (south, east, west, north), whatever the pad prints on them. GameController names
// most pads' face buttons by position (buttonA bottom, buttonB right, buttonX left, buttonY top), but a Nintendo pad's by
// their printed labels (buttonA is the right button): a Switch Pro Controller and Joy-Con (productCategory), as the host's
// pad_apple.h has it. The Rust side maps positions to Switch buttons through controls.json.
//
// Hot-plug: GCControllerDidConnect / DidDisconnect notifications keep the list of pads; the first one connected plays
// player 1. A synthetic pad (GCController controllerWithExtendedGamepad, for runs with no pad in reach) joins the same
// list by the same DidConnect notification, so it goes through the same reading and mapping as a real one.
#import <Foundation/Foundation.h>
#import <GameController/GameController.h>
#include <stdint.h>
#include <string.h>

// Bits of ounce_gc_state.pressed (src/hid/gamepad_mac.rs ELEMENTS)
enum {
    GC_SOUTH, GC_EAST, GC_WEST, GC_NORTH, GC_L1, GC_R1, GC_L2, GC_R2, GC_L3, GC_R3,
    GC_MENU, GC_OPTIONS, GC_DLEFT, GC_DUP, GC_DRIGHT, GC_DDOWN, GC_HOME, GC_SHARE,
};

typedef struct {
    uint32_t pressed;               // GC_* bits
    float lx, ly, rx, ry;           // -1..1, y up
    float lt, rt;                   // 0..1
    uint32_t generation;            // bumps on every connect / disconnect
    int32_t connected;              // pads in the list
    char name[128];                 // player 1's pad
} ounce_gc_state;

static NSMutableArray<GCController *> *g_pads;   // connect order; guarded by @synchronized(g_pads)
static GCController *g_synth;
static NSString *g_synth_name;
static _Atomic uint32_t g_gen;

static int labelled(GCController *c) {
    NSString *k = c.productCategory;
    return [k isEqualToString:@"Switch Pro Controller"] || [k containsString:@"Joy-Con"];
}
static NSString *pad_name(GCController *c) {
    if (c == g_synth) return g_synth_name;
    return c.vendorName ?: c.productCategory ?: @"Controller";
}
static void pad_add(GCController *c) {
    if (!c.extendedGamepad) {
        NSLog(@"[OunceCore] controller connected: %@ (no extended gamepad profile: not used)", pad_name(c));
        return;
    }
    @synchronized(g_pads) {
        if ([g_pads containsObject:c]) return;
        [g_pads addObject:c];
    }
    // Home and Capture would otherwise be macOS gestures (Home opens the Games app)
    for (NSString *b in @[ GCInputButtonHome, GCInputButtonShare ])
        c.physicalInputProfile.buttons[b].preferredSystemGestureState = GCSystemGestureStateDisabled;
    g_gen++;
    printf("[OunceCore] controller connected: %s (%s)\n", pad_name(c).UTF8String, c.productCategory.UTF8String ?: "synthetic");
    fflush(stdout);
}
static void pad_remove(GCController *c) {
    @synchronized(g_pads) {
        if (![g_pads containsObject:c]) return;
        [g_pads removeObject:c];
    }
    g_gen++;
    printf("[OunceCore] controller disconnected: %s\n", pad_name(c).UTF8String);
    fflush(stdout);
}

// Idempotent. Pads already connected join at once; later ones arrive by notification (main queue).
void ounce_gc_start(void) {
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        g_pads = [NSMutableArray new];
        NSNotificationCenter *nc = NSNotificationCenter.defaultCenter;
        [nc addObserverForName:GCControllerDidConnectNotification object:nil queue:nil usingBlock:^(NSNotification *n) { pad_add(n.object); }];
        [nc addObserverForName:GCControllerDidDisconnectNotification object:nil queue:nil usingBlock:^(NSNotification *n) { pad_remove(n.object); }];
        for (GCController *c in GCController.controllers) pad_add(c);
    });
}

static void fill(GCController *c, ounce_gc_state *s) {
    GCExtendedGamepad *g = c.extendedGamepad;
    GCControllerButtonInput *south = g.buttonA, *east = g.buttonB, *west = g.buttonX, *north = g.buttonY;
    if (labelled(c)) { south = g.buttonB; east = g.buttonA; west = g.buttonY; north = g.buttonX; }
    GCControllerButtonInput *by[] = {
        south, east, west, north, g.leftShoulder, g.rightShoulder, g.leftTrigger, g.rightTrigger,
        g.leftThumbstickButton, g.rightThumbstickButton, g.buttonMenu, g.buttonOptions,
        g.dpad.left, g.dpad.up, g.dpad.right, g.dpad.down, g.buttonHome,
        (GCControllerButtonInput *)c.physicalInputProfile.buttons[GCInputButtonShare],
    };
    uint32_t m = 0;
    for (uint32_t i = 0; i < sizeof by / sizeof *by; i++)
        if (by[i].isPressed) m |= 1u << i;   // nil (a pad without that button) is not pressed
    s->pressed = m;
    s->lx = g.leftThumbstick.xAxis.value; s->ly = g.leftThumbstick.yAxis.value;
    s->rx = g.rightThumbstick.xAxis.value; s->ry = g.rightThumbstick.yAxis.value;
    s->lt = g.leftTrigger.value; s->rt = g.rightTrigger.value;
    snprintf(s->name, sizeof s->name, "%s", pad_name(c).UTF8String ?: "Controller");
}

// Player 1's pad (the first connected), or nothing: returns 1 and fills `s`, or 0 with `s` cleared (connected and
// generation still set).
int ounce_gc_poll(ounce_gc_state *s) {
    memset(s, 0, sizeof *s);
    GCController *c;
    @synchronized(g_pads) {
        s->connected = (int32_t)g_pads.count;
        c = g_pads.firstObject;
    }
    s->generation = g_gen;
    if (!c) return 0;
    fill(c, s);
    return 1;
}

// Player 1's pad name into `out` ("" when none). For the library window.
void ounce_gc_name(char *out, size_t cap) {
    ounce_gc_state s;
    if (!ounce_gc_poll(&s)) s.name[0] = 0;
    snprintf(out, cap, "%s", s.name);
}

// A synthetic pad: connects (`name` set) or disconnects (`name` NULL) through GameController's own notifications.
void ounce_gc_synthetic(const char *name) {
    ounce_gc_start();
    void (^go)(void) = ^{
        if (name && !g_synth) {
            g_synth = [GCController controllerWithExtendedGamepad];
            g_synth_name = @(name);
            [NSNotificationCenter.defaultCenter postNotificationName:GCControllerDidConnectNotification object:g_synth];
        } else if (!name && g_synth) {
            [NSNotificationCenter.defaultCenter postNotificationName:GCControllerDidDisconnectNotification object:g_synth];
            g_synth = nil;
        }
    };
    if (NSThread.isMainThread) go(); else dispatch_sync(dispatch_get_main_queue(), go);
}

// Sets the synthetic pad's elements the way a physical pad's would change (GC_* bits by position, sticks -1..1).
void ounce_gc_synthetic_set(uint32_t pressed, float lx, float ly, float rx, float ry) {
    GCExtendedGamepad *g = g_synth.extendedGamepad;
    if (!g) return;
    GCControllerButtonInput *by[] = {
        g.buttonA, g.buttonB, g.buttonX, g.buttonY, g.leftShoulder, g.rightShoulder, g.leftTrigger, g.rightTrigger,
        g.leftThumbstickButton, g.rightThumbstickButton, g.buttonMenu, g.buttonOptions,
    };
    for (uint32_t i = 0; i < sizeof by / sizeof *by; i++) [by[i] setValue:(pressed >> i & 1) ? 1.f : 0.f];
    float dx = (pressed >> GC_DRIGHT & 1) - (float)(pressed >> GC_DLEFT & 1), dy = (pressed >> GC_DUP & 1) - (float)(pressed >> GC_DDOWN & 1);
    [g.dpad setValueForXAxis:dx yAxis:dy];
    [g.leftThumbstick setValueForXAxis:lx yAxis:ly];
    [g.rightThumbstick setValueForXAxis:rx yAxis:ry];
}
