const std = @import("std");
const runner = @import("runner");
const native_sdk = @import("native_sdk");

pub const panic = std.debug.FullPanic(native_sdk.debug.capturePanic);

const App = struct {
    env_map: *std.process.Environ.Map,

    fn app(self: *@This()) native_sdk.App {
        return .{
            .context = self,
            .name ="native-cleaner",
            .source = native_sdk.frontend.productionSource(.{ .dist ="frontend/dist" }),
            .source_fn = source,
        };
    }

    fn source(context: *anyopaque) anyerror!native_sdk.WebViewSource {
        const self: *@This() = @ptrCast(@alignCast(context));
        return native_sdk.frontend.sourceFromEnv(self.env_map, .{
            .dist ="frontend/dist",
            .entry = "index.html",
        });
    }
};

const dev_origins = [_][]const u8{ "zero://app", "zero://inline", "http://127.0.0.1:45831" };
const dialog_policies = [_]native_sdk.BridgeCommandPolicy{
    .{ .name = "native-sdk.dialog.openFile", .origins = &dev_origins },
};

pub fn main(init: std.process.Init) !void {
    var app = App{ .env_map = init.environ_map };
    try runner.runWithOptions(app.app(), .{
        .app_name ="Native Cleaner",
        .window_title ="Native Cleaner",
        .bundle_id ="dev.native_sdk.native-cleaner",
        .icon_path = "assets/icon.png",
        .builtin_bridge = .{ .enabled = true, .commands = &dialog_policies },
        .security = .{
            .navigation = .{ .allowed_origins = &dev_origins },
        },
    }, init);
}

test "app name is configured" {
    try std.testing.expectEqualStrings("native-cleaner","native-cleaner");
}
