package app.tuilauncher.compat;

import cpw.mods.modlauncher.serviceapi.ITransformerDiscoveryService;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.Collections;
import java.util.List;

/** Original MIT-licensed bridge for the ModLauncher 4.x discovery SPI. */
public final class OptiFineDiscovery implements ITransformerDiscoveryService {
    @Override
    public List<Path> candidates(Path gameDirectory) {
        String candidate = System.getProperty("tui.optifine.jar");
        if (candidate == null || candidate.isEmpty()) {
            return Collections.emptyList();
        }
        // One validated JAR path: a path separator inside a directory name is
        // part of the path, rather than a list delimiter.
        return Collections.singletonList(Paths.get(candidate));
    }
}
