package dev.mavenhaste.compat;

import net.fabricmc.api.ModInitializer;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

public final class StonecutterFabric implements ModInitializer {
    private static final Logger LOGGER = LoggerFactory.getLogger("maven-haste-stonecutter-fabric");

    @Override
    public void onInitialize() {
        LOGGER.info("Stonecutter compatibility fixture initialized");
    }
}
