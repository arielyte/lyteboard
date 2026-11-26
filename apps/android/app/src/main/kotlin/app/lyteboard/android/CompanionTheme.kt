package app.lyteboard.android

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.SideEffect
import androidx.compose.ui.platform.LocalView
import androidx.core.view.WindowCompat
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight

private val Light = lightColorScheme(
    background = Color(0xfff8faf8), surface = Color.White,
    onBackground = Color(0xff172923), onSurface = Color(0xff172923),
    surfaceVariant = Color(0xffeef2ef), onSurfaceVariant = Color(0xff53665d),
    surfaceContainer = Color(0xffeef2ef), surfaceContainerLow = Color(0xfff8faf8),
    surfaceContainerHigh = Color(0xffe9f0eb), outline = Color(0xff85948b),
    outlineVariant = Color(0xffdbe3dd), primary = Color(0xff087358), onPrimary = Color.White,
    primaryContainer = Color(0xffe1f2e9), onPrimaryContainer = Color(0xff0c5d47),
    secondary = Color(0xff087358), secondaryContainer = Color(0xffe1f2e9),
    onSecondaryContainer = Color(0xff0c5d47), error = Color(0xffa63838),
)

private val Dark = darkColorScheme(
    background = Color(0xff171d1a), surface = Color(0xff202824),
    onBackground = Color(0xffedf5f0), onSurface = Color(0xffedf5f0),
    surfaceVariant = Color(0xff293630), onSurfaceVariant = Color(0xffaabdb2),
    surfaceContainer = Color(0xff141916), surfaceContainerLow = Color(0xff171d1a),
    surfaceContainerHigh = Color(0xff293630), outline = Color(0xff84988b),
    outlineVariant = Color(0xff344238), primary = Color(0xff81d8b6), onPrimary = Color(0xff102820),
    primaryContainer = Color(0xff253e33), onPrimaryContainer = Color(0xffa0e6cb),
    secondary = Color(0xff81d8b6), secondaryContainer = Color(0xff253e33),
    onSecondaryContainer = Color(0xffa0e6cb), error = Color(0xffffaeaa),
)

private val NotoSans = FontFamily(
    Font(R.font.noto_sans_regular),
    Font(R.font.noto_sans_bold, FontWeight.SemiBold),
    Font(R.font.noto_sans_bold, FontWeight.Bold),
)
private val base = Typography()
private val CompanionTypography = Typography(
    headlineMedium = base.headlineMedium.copy(fontFamily = NotoSans, fontWeight = FontWeight.Bold),
    headlineSmall = base.headlineSmall.copy(fontFamily = NotoSans, fontWeight = FontWeight.Bold),
    titleLarge = base.titleLarge.copy(fontFamily = NotoSans, fontWeight = FontWeight.SemiBold),
    titleMedium = base.titleMedium.copy(fontFamily = NotoSans, fontWeight = FontWeight.SemiBold),
    titleSmall = base.titleSmall.copy(fontFamily = NotoSans, fontWeight = FontWeight.SemiBold),
    bodyLarge = base.bodyLarge.copy(fontFamily = NotoSans),
    bodyMedium = base.bodyMedium.copy(fontFamily = NotoSans),
    bodySmall = base.bodySmall.copy(fontFamily = NotoSans),
    labelLarge = base.labelLarge.copy(fontFamily = NotoSans, fontWeight = FontWeight.SemiBold),
    labelMedium = base.labelMedium.copy(fontFamily = NotoSans),
    labelSmall = base.labelSmall.copy(fontFamily = NotoSans),
)

@Composable
internal fun CompanionTheme(appearance: Appearance, content: @Composable () -> Unit) {
    val dark = when (appearance) {
        Appearance.SYSTEM -> isSystemInDarkTheme()
        Appearance.LIGHT -> false
        Appearance.DARK -> true
    }
    val view = LocalView.current
    val activity = view.context as? android.app.Activity
    SideEffect {
        activity?.let {
            WindowCompat.getInsetsController(it.window, view).apply {
                isAppearanceLightStatusBars = !dark
                isAppearanceLightNavigationBars = !dark
            }
        }
    }
    MaterialTheme(colorScheme = if (dark) Dark else Light, typography = CompanionTypography, content = content)
}
